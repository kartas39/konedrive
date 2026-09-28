#include "fakedaemon.h"
#include "qmlregistration.h"

#include <KLocalizedQmlContext>
#include <KLocalizedString>

#include <QFile>
#include <QLocale>
#include <QQmlApplicationEngine>
#include <QElapsedTimer>
#include <QQuickItem>
#include <QQuickStyle>
#include <QQuickWindow>
#include <QTemporaryDir>
#include <QTest>

/// The window's confirmations act on the account they were opened for.
/// Another account chosen while one is open — by a notification, the tray,
/// or konedrivectl removing the one shown — closes it and never redirects
/// it. Main.qml as the app loads it, offscreen, against the fake daemon;
/// XDG_CONFIG_HOME is a temporary directory.
class DialogsTest : public QObject
{
    Q_OBJECT

private:
    QTemporaryDir m_config;

    static bool shown(const QObject *dialog)
    {
        return dialog->property("visible").toBool();
    }

    /// The visible form rows (delegates) under `root`, by the visual tree,
    /// whose text holds `part`.
    static int itemsSaying(const QQuickItem *root, const QString &part)
    {
        int n = 0;
        const auto children = root->childItems();
        for (const QQuickItem *item : children) {
            if (!item->isVisible()) {
                continue;
            }
            if (QLatin1String(item->metaObject()->className()).contains(QLatin1String("Delegate"))
                && item->property("text").toString().contains(part)) {
                ++n;
            }
            n += itemsSaying(item, part);
        }
        return n;
    }

private Q_SLOTS:
    void initTestCase()
    {
        QVERIFY(m_config.isValid());
        qputenv("XDG_CONFIG_HOME", QFile::encodeName(m_config.path()));
        KLocalizedString::setApplicationDomain("konedrive");
        QQuickStyle::setStyle(QStringLiteral("org.kde.desktop"));
    }

    void aDialogStaysWithItsAccount()
    {
        FakeDaemon fake({QStringLiteral("Personal"), QStringLiteral("Family")});
        for (FakeAccountObject *object : std::as_const(fake.objects)) {
            object->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
            object->sync->set({{QStringLiteral("RootPath"), QString(QStringLiteral("/home/u/") + object->account->label())},
                               {QStringLiteral("RootState"), QStringLiteral("ready")},
                               {QStringLiteral("RootSource"), QStringLiteral("onedrive")}});
        }
        QVERIFY(fake.start());
        // Paths, not the fake's objects: a removed account's object is deleted.
        const QString personal = fake.object(0)->path;
        const QString family = fake.object(1)->path;
        FakeSync1 *personalSync = fake.object(0)->sync;
        FakeSync1 *familySync = fake.object(1)->sync;

        Autostart autostart;
        DownloadProgressSettings progress;
        PlacesSettings places;
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);
        engine.load(QUrl(QStringLiteral("qrc:/Main.qml")));
        QVERIFY(!engine.rootObjects().isEmpty());
        auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
        QVERIFY(window);
        window->show();
        QTRY_COMPARE(accounts.count(), 2);
        QTRY_VERIFY(accounts.at(1)->sync()->serviceAvailable());

        // Free Up Space asked for Family; a notification about Personal chooses it.
        current.select(family);
        QObject *freeUp = window->findChild<QObject *>(QStringLiteral("freeUpDialog"));
        QVERIFY(freeUp);
        QMetaObject::invokeMethod(freeUp, "open");
        QTRY_VERIFY(shown(freeUp));
        current.select(personal);
        QTRY_VERIFY(!shown(freeUp));
        // Confirmed all the same, it frees up the folder it asked about.
        QMetaObject::invokeMethod(freeUp, "confirm");
        QTRY_VERIFY(familySync->calls.contains(QStringLiteral("FreeUpSpace")));
        QVERIFY(!personalSync->calls.contains(QStringLiteral("FreeUpSpace")));

        // Remove, likewise.
        current.select(family);
        QObject *remove = window->findChild<QObject *>(QStringLiteral("removeDialog"));
        QVERIFY(remove);
        QMetaObject::invokeMethod(remove, "open");
        QTRY_VERIFY(shown(remove));
        QCOMPARE(remove->property("title").toString(), QStringLiteral("Remove Family?"));
        current.select(personal);
        QTRY_VERIFY(!shown(remove));
        QMetaObject::invokeMethod(remove, "confirm");
        QTRY_VERIFY(fake.manager->calls.contains(QStringLiteral("Remove:") + family));
        QVERIFY(!fake.manager->calls.contains(QStringLiteral("Remove:") + personal));
        QTRY_COMPARE(accounts.count(), 1);

        // The last account removed elsewhere while its Remove is open: the
        // dialog closes, and has nothing left to remove.
        QMetaObject::invokeMethod(remove, "open");
        QTRY_VERIFY(shown(remove));
        QCOMPARE(remove->property("title").toString(), QStringLiteral("Remove Personal?"));
        fake.removeAccount(personal);
        QTRY_VERIFY(!shown(remove));
        QTRY_COMPARE(remove->property("item").value<QObject *>(), nullptr);
        QMetaObject::invokeMethod(remove, "confirm");
        QTest::qWait(100);
        QVERIFY(!fake.manager->calls.contains(QStringLiteral("Remove:") + personal));

        fake.stop();
    }

    /// "Upload changes made on this computer" follows Mode. Turned on, it
    /// explains before the daemon is asked anything; turned off while
    /// changes wait to be uploaded, it asks before it drops them.
    void theUploadSwitch()
    {
        FakeDaemon fake;
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        fake.sync->set({{QStringLiteral("RootPath"), QStringLiteral("/home/u/OneDrive")},
                        {QStringLiteral("RootState"), QStringLiteral("ready")},
                        {QStringLiteral("RootSource"), QStringLiteral("onedrive")}});
        QVERIFY(fake.start());

        Autostart autostart;
        DownloadProgressSettings progress;
        PlacesSettings places;
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);
        engine.load(QUrl(QStringLiteral("qrc:/Main.qml")));
        QVERIFY(!engine.rootObjects().isEmpty());
        auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
        QVERIFY(window);
        window->show();
        QTRY_COMPARE(accounts.count(), 1);
        QTRY_VERIFY(accounts.at(0)->sync()->serviceAvailable() && accounts.at(0)->account()->state() == QLatin1String("signed-in"));
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("account")));
        QObject *page = window->findChild<QObject *>(QStringLiteral("accountPage"));
        QObject *uploadSwitch = window->findChild<QObject *>(QStringLiteral("uploadSwitch"));
        QObject *explain = window->findChild<QObject *>(QStringLiteral("uploadDialog"));
        QObject *drop = window->findChild<QObject *>(QStringLiteral("dropUploadsDialog"));
        QVERIFY(page && uploadSwitch && explain && drop);
        QTRY_VERIFY(uploadSwitch->property("enabled").toBool());
        QVERIFY(!uploadSwitch->property("checked").toBool());

        // On: explained first. The fake's development gate refuses it, as
        // the daemon's does by default, so no browser opens here.
        QMetaObject::invokeMethod(page, "setUploads", Q_ARG(QVariant, true));
        QTRY_VERIFY(shown(explain));
        QVERIFY(!fake.account->calls.join(QLatin1Char(' ')).contains(QStringLiteral("SetMode")));
        QVERIFY(explain->property("subtitle").toString().contains(QStringLiteral("/home/u/OneDrive")));
        QMetaObject::invokeMethod(explain, "confirm");
        QTRY_VERIFY(fake.account->calls.contains(QStringLiteral("SetMode:read-write:no-force")));
        QTRY_VERIFY(accounts.at(0)->account()->actionError().startsWith(QStringLiteral("Uploading is not available")));
        QVERIFY(!uploadSwitch->property("checked").toBool());

        // It follows the mode the account runs in.
        fake.account->set({{QStringLiteral("Mode"), QStringLiteral("read-write")}});
        QTRY_VERIFY(uploadSwitch->property("checked").toBool());

        // Off, with changes waiting: asked first, then forced.
        fake.account->pendingUploads = 2;
        QMetaObject::invokeMethod(page, "setUploads", Q_ARG(QVariant, false));
        QTRY_VERIFY(shown(drop));
        QVERIFY(fake.account->calls.contains(QStringLiteral("SetMode:read-only:no-force")));
        QTRY_VERIFY(uploadSwitch->property("checked").toBool());
        QMetaObject::invokeMethod(drop, "confirm");
        QTRY_VERIFY(fake.account->calls.contains(QStringLiteral("SetMode:read-only:force")));
        QTRY_COMPARE(accounts.at(0)->account()->mode(), QStringLiteral("read-only"));
        QTRY_VERIFY(accounts.at(0)->account()->switchingTo().isEmpty());
        QVERIFY(!uploadSwitch->property("checked").toBool());

        fake.stop();
    }

    /// Issue #20: with 5000 changes OneDrive has no room for and 3 names it
    /// refuses, both pages come up at once and build a handful of rows — a
    /// line per reason, files only for a per-file reason opened — and the
    /// window never asks for every row.
    void thousandsKeptBackAreAFewLines()
    {
        const QString root = QStringLiteral("/home/u/OneDrive");
        FakeDaemon fake;
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}, {QStringLiteral("Mode"), QStringLiteral("read-write")}});
        fake.sync->set({{QStringLiteral("RootPath"), root},
                        {QStringLiteral("RootState"), QStringLiteral("ready")},
                        {QStringLiteral("RootSource"), QStringLiteral("onedrive")},
                        {QStringLiteral("PendingCount"), QVariant::fromValue<uint>(2)},
                        {QStringLiteral("PendingBytes"), QVariant::fromValue<qulonglong>(1024)},
                        {QStringLiteral("BlockedCount"), QVariant::fromValue<uint>(5003)},
                        {QStringLiteral("DownloadLeftCount"), QVariant::fromValue<uint>(1234)},
                        {QStringLiteral("DownloadLeftBytes"), QVariant::fromValue<qulonglong>(3ULL << 30)},
                        {QStringLiteral("DownloadDoneBytes"), QVariant::fromValue<qulonglong>(1ULL << 30)},
                        {QStringLiteral("DownloadTimeLeft"), QVariant::fromValue<uint>(720)},
                        {QStringLiteral("UploadLeftCount"), QVariant::fromValue<uint>(2)},
                        {QStringLiteral("UploadLeftBytes"), QVariant::fromValue<qulonglong>(1024)},
                        {QStringLiteral("UploadDoneBytes"), QVariant::fromValue<qulonglong>(0)},
                        {QStringLiteral("UploadTimeLeft"), QVariant::fromValue<uint>(0)}});
        fake.sync->keptBack = {{QStringLiteral("one-action"), QStringLiteral("quota-exceeded"), 5000, 5000ULL << 20},
                               {QStringLiteral("per-file"), QStringLiteral("name-characters"), 3, 30}};
        KonedriveSkippedList quota;
        for (int i = 0; i < 5000; ++i) {
            quota << KonedriveSkippedItem{root + QStringLiteral("/big/%1.bin").arg(i), QStringLiteral("quota-exceeded")};
        }
        fake.sync->keptBackFiles.insert(QStringLiteral("quota-exceeded"), quota);
        fake.sync->keptBackFiles.insert(QStringLiteral("name-characters"),
                                        {{root + QStringLiteral("/a:b"), QStringLiteral("name-characters")},
                                         {root + QStringLiteral("/c?d"), QStringLiteral("name-characters")},
                                         {root + QStringLiteral("/e|f"), QStringLiteral("name-characters")}});
        QVERIFY(fake.start());

        Autostart autostart;
        DownloadProgressSettings progress;
        PlacesSettings places;
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);
        engine.load(QUrl(QStringLiteral("qrc:/Main.qml")));
        QVERIFY(!engine.rootObjects().isEmpty());
        auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
        QVERIFY(window);
        window->show();
        QTRY_COMPARE(accounts.count(), 1);
        QTRY_VERIFY(accounts.at(0)->sync()->serviceAvailable() && accounts.at(0)->sync()->blockedCount() == 5003);
        SyncController *sync = accounts.at(0)->sync();

        QElapsedTimer clock;
        clock.start();
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("notUploaded")));
        auto *page = window->findChild<QQuickItem *>(QStringLiteral("notUploadedPage"));
        QVERIFY(page);
        QTRY_VERIFY(sync->notUploadedKnown());
        QTRY_COMPARE(itemsSaying(page, QStringLiteral("changes: OneDrive is full")), 1);
        QVERIFY2(clock.elapsed() < 3000, qPrintable(QString::number(clock.elapsed())));
        QCOMPARE(itemsSaying(page, QStringLiteral("3 files")), 1);
        QCOMPARE(itemsSaying(page, root), 0);
        QCOMPARE(itemsSaying(page, QStringLiteral("Refresh")), 1);
        const int rows = itemsSaying(page, QString());
        QVERIFY2(rows <= 10, qPrintable(QString::number(rows)));

        // Opened, the refused names are listed; the full OneDrive never is.
        QMetaObject::invokeMethod(page, "setOpened", Q_ARG(QVariant, QStringLiteral("name-characters")), Q_ARG(QVariant, true));
        QTRY_COMPARE(itemsSaying(page, root), 3);
        QVERIFY(fake.sync->calls.contains(QStringLiteral("NotUploadedFiles:name-characters:20")));
        QVERIFY(!fake.sync->calls.join(QLatin1Char(' ')).contains(QStringLiteral("NotUploadedFiles:quota-exceeded")));

        // Activity: what is left is in the cards (issue #16), and one line leads to what is kept back.
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("activity")));
        auto *activity = window->findChild<QQuickItem *>(QStringLiteral("activityPage"));
        QVERIFY(activity);
        auto *link = window->findChild<QQuickItem *>(QStringLiteral("keptBackLink"));
        QVERIFY(link);
        QTRY_VERIFY(link->isVisible());
        QTRY_VERIFY2(link->property("text").toString().endsWith(QStringLiteral("003 changes kept back — see Not Uploaded")),
                     qPrintable(link->property("text").toString()));
        const auto text = [window](const char *name) {
            auto *item = window->findChild<QQuickItem *>(QLatin1String(name));
            return item && item->isVisible() ? item->property("text").toString() : QString();
        };
        // Sizes as the locale writes them, and 1234 with or without its thousands apart.
        const QLocale locale;
        const QString down = text("downloadingCardLeft");
        QVERIFY2(down.startsWith(QLatin1Char('1'))
                     && down.endsWith(QStringLiteral("234 files left · ") + locale.formattedDataSize(3ULL << 30) + QStringLiteral(" · about 12 min")),
                 qPrintable(down));
        QCOMPARE(text("downloadingCardDone"), locale.formattedDataSize(1ULL << 30) + QStringLiteral(" done"));
        QCOMPARE(text("uploadingCardLeft"), QStringLiteral("2 changes left · ") + locale.formattedDataSize(1024));
        QCOMPARE(text("uploadingCardDone"), locale.formattedDataSize(0) + QStringLiteral(" done"));
        QCOMPARE(itemsSaying(activity, root), 0);
        QVERIFY(itemsSaying(activity, QString()) <= 10);

        // Nothing left: the lines go.
        fake.sync->set({{QStringLiteral("DownloadLeftCount"), QVariant::fromValue<uint>(0)}});
        QTRY_VERIFY(text("downloadingCardLeft").isEmpty());
        QVERIFY(text("downloadingCardDone").isEmpty());
        QVERIFY(!text("uploadingCardLeft").isEmpty());

        QVERIFY(!fake.sync->calls.contains(QStringLiteral("Outbox")));
        QVERIFY(!fake.sync->calls.contains(QStringLiteral("NotUploaded")));
        fake.stop();
    }
};

QTEST_MAIN(DialogsTest)

#include "dialogstest.moc"
