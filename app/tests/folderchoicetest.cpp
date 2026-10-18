#include "accountsmodel.h"
#include "currentaccount.h"
#include "daemoncontroller.h"
#include "fakedaemon.h"
#include "folderchoice.h"
#include "foldertext.h"

#include <QDir>
#include <QFile>
#include <QFileInfo>
#include <QSet>
#include <QTemporaryDir>
#include <QTest>

/// The folder a OneDrive is offered, as a function of what is free, and how the
/// window writes a folder; then FolderChoice against the fake daemon: what it offers
/// the account shown, and what Continue asks. HOME is a temporary directory.
class FolderChoiceTest : public QObject
{
    Q_OBJECT

private:
    QTemporaryDir m_home;
    QByteArray m_realHome;

    static QString suggest(const QString &name, const QSet<QString> &used)
    {
        return ::suggestFolder(QStringLiteral("/home/u"), name, [&used](const QString &path) {
            return !used.contains(path);
        });
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

    void suggestFolder_data()
    {
        QTest::addColumn<QString>("name");
        QTest::addColumn<QStringList>("used");
        QTest::addColumn<QString>("offered");
        QTest::newRow("~/OneDrive is free") << QStringLiteral("ann@example.com") << QStringList() << QStringLiteral("/home/u/OneDrive");
        QTest::newRow("taken: the name up to its @") << QStringLiteral("ann@example.com") << QStringList{QStringLiteral("/home/u/OneDrive")}
                                                     << QStringLiteral("/home/u/OneDrive-ann");
        QTest::newRow("a label, not an email") << QStringLiteral("Work") << QStringList{QStringLiteral("/home/u/OneDrive")} << QStringLiteral("/home/u/OneDrive-Work");
        QTest::newRow("that taken too: numbered") << QStringLiteral("ann@example.com")
                                                  << QStringList{QStringLiteral("/home/u/OneDrive"), QStringLiteral("/home/u/OneDrive-ann")}
                                                  << QStringLiteral("/home/u/OneDrive-ann-2");
        QTest::newRow("no name: numbered") << QString() << QStringList{QStringLiteral("/home/u/OneDrive")} << QStringLiteral("/home/u/OneDrive-2");
    }

    void suggestFolder()
    {
        QFETCH(QString, name);
        QFETCH(QStringList, used);
        QFETCH(QString, offered);
        QCOMPARE(suggest(name, QSet<QString>(used.cbegin(), used.cend())), offered);
    }

    /// None free up to -99: nothing is offered, and the picker is the way.
    void nothingFreeOffersNothing()
    {
        QCOMPARE(::suggestFolder(QStringLiteral("/home/u"), QStringLiteral("ann@example.com"), [](const QString &) {
                     return false;
                 }),
                 QString());
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

    /// Free: not an account's, and nothing there or an empty directory.
    void whatIsFree()
    {
        const QString home = m_home.path();
        QVERIFY(isFreeFolder(home + QStringLiteral("/none"), {}));
        QVERIFY(QDir().mkdir(home + QStringLiteral("/empty")));
        QVERIFY(isFreeFolder(home + QStringLiteral("/empty"), {}));
        QVERIFY(!isFreeFolder(home + QStringLiteral("/empty"), {home + QStringLiteral("/empty")}));
        QVERIFY(QDir().mkdir(home + QStringLiteral("/full")));
        QFile file(home + QStringLiteral("/full/.hidden"));
        QVERIFY(file.open(QIODevice::WriteOnly));
        file.close();
        QVERIFY(!isFreeFolder(home + QStringLiteral("/full"), {}));
        QVERIFY(!isFreeFolder(home + QStringLiteral("/full/.hidden"), {}));
        QVERIFY(QFile::link(home + QStringLiteral("/empty"), home + QStringLiteral("/link")));
        QVERIFY(!isFreeFolder(home + QStringLiteral("/link"), {}));
    }

    /// The account shown is offered the first free folder; one chosen in the picker stays
    /// until another account is shown; a folder freed is offered again.
    void theAccountShownIsOffered()
    {
        const QString home = m_home.path();
        FakeDaemon fake({QStringLiteral("ann@example.com"), QStringLiteral("Family")});
        fake.object(0)->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}, {QStringLiteral("Email"), QStringLiteral("ann@example.com")}});
        fake.object(1)->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        fake.object(1)->sync->folder->set({{QStringLiteral("Path"), QString(home + QStringLiteral("/OneDrive"))},
                                           {QStringLiteral("State"), QStringLiteral("ready")},
                                           {QStringLiteral("Source"), QStringLiteral("onedrive")}});
        QVERIFY(fake.start());
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        FolderChoice choice(&accounts, &current);
        QTRY_COMPARE(accounts.count(), 2);
        current.select(fake.object(0)->path);
        QTRY_COMPARE(accounts.at(1)->sync()->rootPath(), home + QStringLiteral("/OneDrive"));
        QTRY_COMPARE(choice.folder(), home + QStringLiteral("/OneDrive-ann"));
        QCOMPARE(choice.folderText(), QStringLiteral("~/OneDrive-ann"));

        // Family's folder forgotten: ~/OneDrive is free again.
        fake.object(1)->sync->folder->set({{QStringLiteral("Path"), QString()}, {QStringLiteral("State"), QStringLiteral("none")}});
        QTRY_COMPARE(choice.folder(), home + QStringLiteral("/OneDrive"));

        // Chosen in the picker: kept while the account is shown.
        choice.setFolder(QUrl::fromLocalFile(QStringLiteral("/mnt/data/OneDrive")));
        QCOMPARE(choice.folder(), QStringLiteral("/mnt/data/OneDrive"));
        fake.object(1)->sync->folder->set({{QStringLiteral("Path"), QString(home + QStringLiteral("/OneDrive"))}, {QStringLiteral("State"), QStringLiteral("ready")}});
        QTRY_COMPARE(accounts.at(1)->sync()->rootPath(), home + QStringLiteral("/OneDrive"));
        QCOMPARE(choice.folder(), QStringLiteral("/mnt/data/OneDrive"));

        // An account with a folder is offered none.
        current.select(fake.object(1)->path);
        QCOMPARE(choice.folder(), QString());
        current.select(fake.object(0)->path);
        QCOMPARE(choice.folder(), home + QStringLiteral("/OneDrive-ann"));
        fake.stop();
    }

    /// Continue, read-only: the folder is made and registered. Two-way: the switch first, and
    /// the folder only once it is granted; a switch refused registers nothing.
    void continueRegistersTheFolder()
    {
        const QString home = m_home.path();
        FakeDaemon fake;
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        QVERIFY(fake.start());
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        FolderChoice choice(&accounts, &current);
        QTRY_COMPARE(accounts.count(), 1);
        QTRY_VERIFY(accounts.at(0)->account()->state() == QLatin1String("signed-in"));
        QTRY_COMPARE(choice.folder(), home + QStringLiteral("/OneDrive"));

        // Two-way refused: nothing is registered.
        fake.account->readWriteFails = QStringLiteral("cannot listen on localhost");
        choice.choose(true);
        QTRY_VERIFY(fake.account->calls.contains(QStringLiteral("SetMode:read-write:no-force")));
        QTRY_VERIFY(!accounts.at(0)->account()->actionError().isEmpty());
        QTest::qWait(100);
        QVERIFY(!fake.sync->calls.join(QLatin1Char(' ')).contains(QStringLiteral("Register")));

        // Two-way: waits for the browser, then registers.
        fake.account->readWriteFails.clear();
        fake.account->calls.clear();
        choice.choose(true);
        QTRY_VERIFY(accounts.at(0)->account()->modeSignInPending());
        QVERIFY(!fake.sync->calls.join(QLatin1Char(' ')).contains(QStringLiteral("Register")));
        fake.account->grantReadWrite();
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("Register:") + home + QStringLiteral("/OneDrive")));
        QVERIFY(QFileInfo(home + QStringLiteral("/OneDrive")).isDir());

        // Read-only for a two-way account: switched, then registered.
        fake.sync->folder->set({{QStringLiteral("Path"), QString()}, {QStringLiteral("State"), QStringLiteral("none")}});
        QTRY_VERIFY(accounts.at(0)->sync()->rootPath().isEmpty());
        QTRY_COMPARE(accounts.at(0)->account()->mode(), QStringLiteral("read-write"));
        fake.sync->calls.clear();
        choice.choose(false);
        QTRY_VERIFY(fake.account->calls.contains(QStringLiteral("SetMode:read-only:no-force")));
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("Register:") + home + QStringLiteral("/OneDrive")));
        fake.stop();
    }
};

QTEST_MAIN(FolderChoiceTest)

#include "folderchoicetest.moc"
