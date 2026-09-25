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

/// The manager's side of the window: DaemonController (Accounts1),
/// AccountsModel following Accounts, CurrentAccount and its remembered
/// choice, and the Add dialog's one step. XDG_CONFIG_HOME is a temporary
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

    /// One row per account, in the daemon's order, following Add and Remove
    /// made elsewhere (konedrivectl, say).
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

    /// HelperState (dbus/org.konedrive.Accounts1.xml) reaches the window and
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
        family->sync->set({{QStringLiteral("RootPath"), QStringLiteral("/home/u/Family")},
                           {QStringLiteral("RootState"), QStringLiteral("ready")},
                           {QStringLiteral("ConflictCount"), QVariant::fromValue<uint>(1)}});
        QTRY_VERIFY(current.othersNeedAttention());
        QCOMPARE(model.data(model.index(1, 0), AccountsModel::IconNameRole).toString(), QStringLiteral("state-warning"));

        // Shown, it is no longer "another".
        current.select(family->path);
        QVERIFY(!current.othersNeedAttention());
    }

    /// The Add dialog's one step: the client id when none is set, Add, the
    /// new account chosen, and its sign-in opened in the browser.
    void addingSetsTheClientIdAddsChoosesAndSignsIn()
    {
        start({});
        DaemonController daemon;
        AccountsModel model(&daemon);
        CurrentAccount current(&model);
        QTRY_VERIFY(daemon.serviceAvailable());
        QCOMPARE(model.suggestedLabel(), QStringLiteral("Personal"));

        QSignalSpy added(&model, &AccountsModel::accountAdded);
        QSignalSpy open(&model, &AccountsModel::openUrlRequested);
        model.addAccount(QStringLiteral(" Personal "), ClientId);
        QVERIFY(model.adding());
        QTRY_COMPARE(added.count(), 1);
        const QString path = fake::FirstAccount;
        QCOMPARE(added.at(0).at(0).toString(), path);
        QVERIFY(!model.adding());
        QCOMPARE(model.addError(), QString());
        QCOMPARE(m_daemon->manager->calls, (QStringList{QStringLiteral("SetClientId:") + ClientId, QStringLiteral("Add:Personal")}));
        QCOMPARE(current.path(), path);
        QCOMPARE(remembered(), fake::idFor(1));

        QTRY_COMPARE(open.count(), 1);
        QCOMPARE(open.at(0).at(0).toString(), QStringLiteral("https://login.example/authorize?account=") + fake::idFor(1));
        QTRY_COMPARE(current.account()->state(), QStringLiteral("signing-in"));
        QVERIFY(model.anySignedIn());

        // "Personal" is taken now: nothing is suggested.
        QTRY_COMPARE(model.count(), 1);
        QTRY_COMPARE(model.at(0)->account()->label(), QStringLiteral("Personal"));
        QCOMPARE(model.suggestedLabel(), QString());

        // With a client id already set, Add goes straight to Add.
        m_daemon->manager->calls.clear();
        model.addAccount(QStringLiteral("Family"), ClientId);
        QTRY_COMPARE(added.count(), 2);
        QCOMPARE(m_daemon->manager->calls, (QStringList{QStringLiteral("Add:Family")}));
        QCOMPARE(current.account()->label(), QStringLiteral("Family"));
    }

    void aRefusedAddSaysWhy()
    {
        start({});
        DaemonController daemon;
        AccountsModel model(&daemon);
        QTRY_VERIFY(daemon.serviceAvailable());
        QSignalSpy added(&model, &AccountsModel::accountAdded);

        model.addAccount(QStringLiteral("Personal"), QStringLiteral("bad"));
        QTRY_VERIFY(!model.adding());
        QVERIFY2(model.addError().contains(QStringLiteral("invalid client ID")), qPrintable(model.addError()));
        QVERIFY(!m_daemon->manager->calls.contains(QStringLiteral("Add:Personal")));

        m_daemon->addAccount(QStringLiteral("Personal"));
        QTRY_COMPARE(model.count(), 1);
        model.addAccount(QStringLiteral("personal"));
        QTRY_VERIFY(!model.adding());
        QVERIFY2(model.addError().contains(QStringLiteral("taken")), qPrintable(model.addError()));
        QCOMPARE(added.count(), 0);

        // The dialog opens clean next time.
        model.clearAddError();
        QCOMPARE(model.addError(), QString());
    }

    /// Accounts1.Add's rules, checked before the daemon is asked.
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
        QVERIFY(!model.labelProblem(QStringLiteral("ann@home")).isEmpty());
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
