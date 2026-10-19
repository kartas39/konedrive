#include "accountsmodel.h"
#include "currentaccount.h"
#include "daemoncontroller.h"
#include "fakedaemon.h"
#include "folderchoice.h"
#include "foldertext.h"

#include <QDir>
#include <QFile>
#include <QFileInfo>
#include <QTemporaryDir>
#include <QTest>

/// What a folder picked for an account is, a case each, and how the window writes a folder;
/// then FolderChoice against the fake daemon: a folder given, one refused, and another
/// folder for one that stopped. HOME is a temporary directory.
class FolderChoiceTest : public QObject
{
    Q_OBJECT

private:
    QTemporaryDir m_home;
    QByteArray m_realHome;

    QString inHome(const QString &name) const
    {
        return m_home.path() + QLatin1Char('/') + name;
    }

    /// A directory `name` in HOME, with a hidden file in it when `withFile`.
    QString makeDirectory(const QString &name, bool withFile) const
    {
        const QString path = inHome(name);
        if (!QDir().mkdir(path)) {
            return QString();
        }
        if (withFile) {
            QFile file(path + QStringLiteral("/.hidden"));
            if (!file.open(QIODevice::WriteOnly)) {
                return QString();
            }
        }
        return path;
    }

private Q_SLOTS:
    void initTestCase()
    {
        QVERIFY(m_home.isValid());
        m_realHome = qgetenv("HOME");
        qputenv("HOME", QFile::encodeName(m_home.path()));
    }

    void cleanupTestCase()
    {
        qputenv("HOME", m_realHome);
    }

    void init()
    {
        QDir(m_home.path()).removeRecursively();
        QDir().mkpath(m_home.path());
    }

    void folderText_data()
    {
        QTest::addColumn<QString>("path");
        QTest::addColumn<QString>("text");
        QTest::newRow("under home") << QStringLiteral("/home/u/OneDrive") << QStringLiteral("~/OneDrive");
        QTest::newRow("home itself") << QStringLiteral("/home/u") << QStringLiteral("~");
        QTest::newRow("a name that starts like home") << QStringLiteral("/home/user2/OneDrive") << QStringLiteral("/home/user2/OneDrive");
        QTest::newRow("elsewhere") << QStringLiteral("/mnt/data/OneDrive") << QStringLiteral("/mnt/data/OneDrive");
    }

    void folderText()
    {
        QFETCH(QString, path);
        QFETCH(QString, text);
        QCOMPARE(::folderText(path, QStringLiteral("/home/u")), text);
    }

    void nothingThereIsFree()
    {
        QCOMPARE(checkFolder(inHome(QStringLiteral("none")), {}), FolderCheck::Free);
    }

    void anEmptyDirectoryIsFree()
    {
        QCOMPARE(checkFolder(makeDirectory(QStringLiteral("empty"), false), {}), FolderCheck::Free);
    }

    /// A hidden file counts; so does a file in the folder's place, and a link.
    void anythingThereIsNotEmpty()
    {
        const QString full = makeDirectory(QStringLiteral("full"), true);
        QCOMPARE(checkFolder(full, {}), FolderCheck::NotEmpty);
        QCOMPARE(checkFolder(full + QStringLiteral("/.hidden"), {}), FolderCheck::NotEmpty);
        QVERIFY(QFile::link(makeDirectory(QStringLiteral("empty"), false), inHome(QStringLiteral("link"))));
        QCOMPARE(checkFolder(inHome(QStringLiteral("link")), {}), FolderCheck::NotEmpty);
    }

    void anotherAccountsFolderIsTaken()
    {
        const QString other = makeDirectory(QStringLiteral("OneDrive"), false);
        QCOMPARE(checkFolder(other + QLatin1Char('/'), {other}), FolderCheck::AnotherAccountsFolder);
    }

    /// Inside another account's folder, also when it is empty; a name that only starts like
    /// it is not inside.
    void aFolderInsideAnotherAccountsIsTaken()
    {
        const QString other = inHome(QStringLiteral("OneDrive"));
        QCOMPARE(checkFolder(other + QStringLiteral("/Work"), {other}), FolderCheck::InsideAnotherAccountsFolder);
        QCOMPARE(checkFolder(other + QStringLiteral("-Work"), {other}), FolderCheck::Free);
    }

    /// A free folder is made and registered, the one picked for the account; one that is not
    /// empty registers nothing and says why while its account is shown.
    void aFolderPickedIsGivenOrRefused()
    {
        FakeDaemon fake({QStringLiteral("Personal"), QStringLiteral("Family")});
        for (FakeAccountObject *object : std::as_const(fake.objects)) {
            object->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        }
        QVERIFY(fake.start());
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        FolderChoice choice(&accounts, &current);
        QTRY_COMPARE(accounts.count(), 2);
        current.select(fake.object(0)->path);
        AccountItem *personal = accounts.at(0);

        const QString full = makeDirectory(QStringLiteral("full"), true);
        QVERIFY(!choice.choose(personal, QUrl::fromLocalFile(full)));
        QCOMPARE(choice.refusal(), QStringLiteral("~/full is not an empty folder. Choose an empty folder for your OneDrive."));
        // The refusal is Personal's: it shows with Personal only.
        current.select(fake.object(1)->path);
        QCOMPARE(choice.refusal(), QString());
        current.select(fake.object(0)->path);
        QVERIFY(!choice.refusal().isEmpty());

        const QString folder = inHome(QStringLiteral("OneDrive"));
        QVERIFY(choice.choose(personal, QUrl::fromLocalFile(folder)));
        QCOMPARE(choice.refusal(), QString());
        QTRY_VERIFY(fake.object(0)->sync->calls.contains(QStringLiteral("Register:") + folder));
        QVERIFY(QFileInfo(folder).isDir());
        QCOMPARE(fake.object(0)->sync->calls.count(QStringLiteral("Register:") + folder), 1);
        QVERIFY(!fake.object(0)->sync->calls.contains(QStringLiteral("Unregister")));

        // Personal's folder is taken for Family.
        QTRY_COMPARE(personal->sync()->rootPath(), folder);
        QVERIFY(!choice.choose(accounts.at(1), QUrl::fromLocalFile(folder)));
        current.select(fake.object(1)->path);
        QCOMPARE(choice.refusal(), QStringLiteral("~/OneDrive is already the folder of another OneDrive. Choose another folder."));
        QVERIFY(!fake.object(1)->sync->calls.join(QLatin1Char(' ')).contains(QStringLiteral("Register")));
        fake.stop();
    }

    /// A folder that works is never forgotten for one picked: the picked one is only asked
    /// for, and the daemon refuses it.
    void aWorkingFolderIsNotForgotten()
    {
        FakeDaemon fake;
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        fake.sync->folder->set({{QStringLiteral("Path"), inHome(QStringLiteral("Works"))}, {QStringLiteral("State"), QStringLiteral("ready")}});
        fake.sync->folder->decide("ok", "up-to-date");
        QVERIFY(fake.start());
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        FolderChoice choice(&accounts, &current);
        QTRY_COMPARE(accounts.count(), 1);
        QTRY_COMPARE(accounts.at(0)->sync()->overallReason(), QStringLiteral("up-to-date"));

        const QString folder = inHome(QStringLiteral("OneDrive"));
        QVERIFY(choice.choose(accounts.at(0), QUrl::fromLocalFile(folder)));
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("Register:") + folder));
        QVERIFY(!fake.sync->calls.contains(QStringLiteral("Unregister")));
        fake.stop();
    }

    /// For a folder that stopped, the one picked is registered once the stopped one is forgotten.
    void aFolderThatStoppedIsForgottenFirst()
    {
        FakeDaemon fake;
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        fake.sync->folder->set({{QStringLiteral("Path"), inHome(QStringLiteral("Gone"))}, {QStringLiteral("State"), QStringLiteral("error")}});
        fake.sync->folder->decide("warning", "stopped");
        QVERIFY(fake.start());
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        FolderChoice choice(&accounts, &current);
        QTRY_COMPARE(accounts.count(), 1);
        QTRY_VERIFY(!accounts.at(0)->sync()->rootPath().isEmpty());

        const QString folder = inHome(QStringLiteral("OneDrive"));
        QVERIFY(choice.choose(accounts.at(0), QUrl::fromLocalFile(folder)));
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("Register:") + folder));
        QCOMPARE(fake.sync->calls.count(QStringLiteral("Unregister")), 1);
        QVERIFY(fake.sync->calls.indexOf(QStringLiteral("Unregister")) < fake.sync->calls.indexOf(QStringLiteral("Register:") + folder));
        fake.stop();
    }
};

QTEST_MAIN(FolderChoiceTest)

#include "folderchoicetest.moc"
