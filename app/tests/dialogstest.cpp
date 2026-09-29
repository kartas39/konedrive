#include "fakedaemon.h"
#include "qmlregistration.h"

#include <KLocalizedQmlContext>
#include <KLocalizedString>

#include <QDateTime>
#include <QFile>
#include <QLocale>
#include <QQmlApplicationEngine>
#include <QElapsedTimer>
#include <QQuickItem>
#include <QQuickStyle>
#include <QQuickWindow>
#include <QRegularExpression>
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

    /// The delegates a ListView has built: its content item's children
    /// named `name` (pooled ones included).
    static int builtRows(const QQuickItem *list, const QString &name)
    {
        const auto *content = list->property("contentItem").value<QQuickItem *>();
        int n = 0;
        const auto children = content ? content->childItems() : QList<QQuickItem *>();
        for (const QQuickItem *item : children) {
            n += item->objectName() == name;
        }
        return n;
    }

    /// The digits of `text`, so a count reads the same with or without its thousands apart.
    static QString digits(QString text)
    {
        return text.remove(QRegularExpression(QStringLiteral("[^0-9]")));
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
            object->sync->folder->set({{QStringLiteral("Path"), QString(QStringLiteral("/home/u/") + object->account->label())},
                                       {QStringLiteral("State"), QStringLiteral("ready")},
                                       {QStringLiteral("Source"), QStringLiteral("onedrive")}});
        }
        QVERIFY(fake.start());
        // Paths, not the fake's objects: a removed account's object is deleted.
        const QString personal = fake.object(0)->path;
        const QString family = fake.object(1)->path;
        FakeSync *personalSync = fake.object(0)->sync;
        FakeSync *familySync = fake.object(1)->sync;

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
        fake.sync->folder->set({{QStringLiteral("Path"), QStringLiteral("/home/u/OneDrive")},
                                {QStringLiteral("State"), QStringLiteral("ready")},
                                {QStringLiteral("Source"), QStringLiteral("onedrive")}});
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

    /// Issues #57, #80: the account page's sync settings show what the daemon says and set
    /// it; the thumbnails line shows while they are off.
    void theSyncSettings()
    {
        FakeDaemon fake;
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        fake.sync->folder->set({{QStringLiteral("Path"), QStringLiteral("/home/u/OneDrive")},
                                {QStringLiteral("State"), QStringLiteral("ready")},
                                {QStringLiteral("Source"), QStringLiteral("onedrive")}});
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
        auto *thumbnails = window->findChild<QQuickItem *>(QStringLiteral("thumbnailsSwitch"));
        auto *offLine = window->findChild<QQuickItem *>(QStringLiteral("thumbnailsOffLine"));
        auto *metered = window->findChild<QQuickItem *>(QStringLiteral("meteredSwitch"));
        auto *battery = window->findChild<QQuickItem *>(QStringLiteral("batteryCombo"));
        QVERIFY(thumbnails && offLine && metered && battery);
        QTRY_VERIFY(thumbnails->isVisible());
        QVERIFY(thumbnails->property("checked").toBool());
        QVERIFY(!offLine->isVisible());
        QVERIFY(metered->property("checked").toBool());
        QCOMPARE(battery->property("currentIndex").toInt(), 1);

        // A switch turned by the user asks the daemon; the daemon's answer is what shows.
        thumbnails->setProperty("checked", false);
        QMetaObject::invokeMethod(thumbnails, "toggled");
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("SetThumbnails:off")));
        QTRY_VERIFY(!thumbnails->property("checked").toBool());
        QTRY_VERIFY(offLine->isVisible());
        metered->setProperty("checked", false);
        QMetaObject::invokeMethod(metered, "toggled");
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("SetPauseOnMetered:off")));
        QTRY_VERIFY(!metered->property("checked").toBool());
        QMetaObject::invokeMethod(battery, "activated", Q_ARG(int, 2));
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("SetOnBattery:pause")));
        QTRY_COMPARE(battery->property("currentIndex").toInt(), 2);

        // Changed elsewhere (konedrivectl), the page follows.
        fake.sync->folder->set({{QStringLiteral("Thumbnails"), true}, {QStringLiteral("OnBattery"), QStringLiteral("sync")}});
        QTRY_VERIFY(thumbnails->property("checked").toBool());
        QTRY_VERIFY(!offLine->isVisible());
        QTRY_COMPARE(battery->property("currentIndex").toInt(), 0);
        fake.stop();
    }

    /// Issue #57: while the account holds back by itself, the Status page says why, beside
    /// a Sync Anyway button; the user's own pause is not shown for it.
    void theStatusPageShowsAHold()
    {
        FakeDaemon fake;
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        fake.sync->folder->set({{QStringLiteral("Path"), QStringLiteral("/home/u/OneDrive")},
                                {QStringLiteral("State"), QStringLiteral("ready")},
                                {QStringLiteral("Source"), QStringLiteral("onedrive")},
                                {QStringLiteral("HeldBack"), QStringLiteral("metered")}});
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
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("status")));
        auto *held = window->findChild<QQuickItem *>(QStringLiteral("heldBackLine"));
        auto *anyway = window->findChild<QObject *>(QStringLiteral("syncAnywayButton"));
        auto *paused = window->findChild<QQuickItem *>(QStringLiteral("pausedLine"));
        QVERIFY(held && anyway && paused);
        QTRY_VERIFY(held->isVisible());
        QCOMPARE(held->property("text").toString(), QStringLiteral("Paused: metered connection"));
        QVERIFY(!paused->isVisible());

        fake.sync->folder->set({{QStringLiteral("HeldBack"), QStringLiteral("power-saver")}});
        QTRY_COMPARE(held->property("text").toString(), QStringLiteral("Paused: power-saver mode"));
        QMetaObject::invokeMethod(anyway, "clicked");
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("SyncAnyway")));
        QTRY_VERIFY(!held->isVisible());
        fake.stop();
    }

    /// Issue #8: the Status page says how the local scan goes — how far a running one got,
    /// of about how many, since when and why; then when the last one finished; and nothing
    /// for a read-only folder.
    void theStatusPageShowsTheLocalScan()
    {
        FakeDaemon fake;
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}, {QStringLiteral("Mode"), QStringLiteral("read-write")}});
        fake.sync->folder->set({{QStringLiteral("Path"), QStringLiteral("/home/u/OneDrive")},
                                {QStringLiteral("State"), QStringLiteral("ready")},
                                {QStringLiteral("Source"), QStringLiteral("onedrive")}});
        fake.sync->scan->set({{QStringLiteral("State"), QStringLiteral("running")},
                              {QStringLiteral("Reason"), QStringLiteral("read-write")},
                              {QStringLiteral("Started"), QVariant::fromValue<qlonglong>(QDateTime::currentSecsSinceEpoch() - 125)},
                              {QStringLiteral("Directories"), QVariant::fromValue<qulonglong>(12)},
                              {QStringLiteral("Files"), QVariant::fromValue<qulonglong>(345)},
                              {QStringLiteral("Expected"), QVariant::fromValue<qulonglong>(500)}});
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
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("status")));
        auto *line = window->findChild<QQuickItem *>(QStringLiteral("scanLine"));
        QVERIFY(line);
        QTRY_VERIFY(line->isVisible());
        QCOMPARE(line->property("text").toString(),
                 QStringLiteral("Checking local files: 12 folders and 345 files, of about 500 — started 2 min ago, after the switch to read-write"));

        fake.sync->scan->set({{QStringLiteral("State"), QStringLiteral("idle")},
                              {QStringLiteral("Finished"), QVariant::fromValue<qlonglong>(QDateTime::currentSecsSinceEpoch() - 300)},
                              {QStringLiteral("Took"), QVariant::fromValue<uint>(40)}});
        QTRY_COMPARE(line->property("text").toString(), QStringLiteral("Local files last checked 5 min ago (took 40 s)"));

        fake.account->set({{QStringLiteral("Mode"), QStringLiteral("read-only")}});
        fake.sync->scan->set({{QStringLiteral("State"), QStringLiteral("none")}});
        QTRY_VERIFY(!line->isVisible());
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
        fake.sync->folder->set({{QStringLiteral("Path"), root},
                                {QStringLiteral("State"), QStringLiteral("ready")},
                                {QStringLiteral("Source"), QStringLiteral("onedrive")}});
        fake.sync->queue->set({{QStringLiteral("PendingCount"), QVariant::fromValue<uint>(2)},
                               {QStringLiteral("PendingBytes"), QVariant::fromValue<qulonglong>(1024)},
                               {QStringLiteral("BlockedCount"), QVariant::fromValue<uint>(5003)}});
        fake.sync->transfers->set({{QStringLiteral("DownloadLeftCount"), QVariant::fromValue<uint>(1234)},
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
        fake.sync->transfers->set({{QStringLiteral("DownloadLeftCount"), QVariant::fromValue<uint>(0)}});
        QTRY_VERIFY(text("downloadingCardLeft").isEmpty());
        QVERIFY(text("downloadingCardDone").isEmpty());
        QVERIFY(!text("uploadingCardLeft").isEmpty());

        QVERIFY(!fake.sync->calls.contains(QStringLiteral("Changes")));
        QVERIFY(!fake.sync->calls.contains(QStringLiteral("NotUploaded")));
        fake.stop();
    }

    /// Issue #39: 5 000 skipped entries and 5 000 conflicts are the first
    /// 200 of each in a list that builds only the rows in sight, then "and
    /// 4800 more"; a count that keeps moving asks for the skipped list again
    /// at most once a second.
    void thousandsSkippedAndConflictsAreAFewRows()
    {
        const QString root = QStringLiteral("/home/u/OneDrive");
        FakeDaemon fake;
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        fake.sync->folder->set({{QStringLiteral("Path"), root},
                                {QStringLiteral("State"), QStringLiteral("ready")},
                                {QStringLiteral("Source"), QStringLiteral("onedrive")},
                                {QStringLiteral("SkippedCount"), QVariant::fromValue<qulonglong>(5000)}});
        fake.sync->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(5000)}});
        fake.sync->skippedList.clear();
        for (int i = 0; i < 5000; ++i) {
            fake.sync->skippedList << KonedriveSkippedItem{root + QStringLiteral("/Shared/%1").arg(i), QStringLiteral("shared")};
            fake.sync->conflictList << KonedriveConflict{i, root + QStringLiteral("/%1.txt").arg(i), root + QStringLiteral("/%1-fedora.txt").arg(i), QStringLiteral("copy")};
        }
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
        QTRY_VERIFY(accounts.at(0)->sync()->serviceAvailable() && accounts.at(0)->sync()->skippedCount() == 5000);
        SyncController *sync = accounts.at(0)->sync();

        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("skipped")));
        auto *skipped = window->findChild<QQuickItem *>(QStringLiteral("skippedList"));
        QVERIFY(skipped);
        QTRY_COMPARE(sync->skipped().size(), 5000);
        QTRY_COMPARE(skipped->property("count").toInt(), 200);
        // The window shows about ten rows; a few more are built beyond them.
        QTRY_VERIFY(builtRows(skipped, QStringLiteral("skippedRow")) >= 5);
        QTest::qWait(100);
        QVERIFY2(builtRows(skipped, QStringLiteral("skippedRow")) <= 40, "more rows built than a few screens hold");
        // At the end, the line that counts the rest.
        QMetaObject::invokeMethod(skipped, "positionViewAtEnd");
        auto *skippedMore = window->findChild<QQuickItem *>(QStringLiteral("skippedMore"));
        QVERIFY(skippedMore);
        QTRY_VERIFY(skippedMore->isVisible());
        QTest::qWait(100);
        QVERIFY2(builtRows(skipped, QStringLiteral("skippedRow")) <= 40, "more rows built than a few screens hold");
        QCOMPARE(digits(skippedMore->property("text").toString()), QStringLiteral("4800"));
        QVERIFY(skippedMore->property("description").toString().contains(QStringLiteral("konedrivectl sync skipped")));

        // Three listing pages in a row: one more Skipped() a second later.
        const auto asked = fake.sync->calls.count(QStringLiteral("Skipped"));
        for (qulonglong count = 5001; count <= 5003; ++count) {
            fake.sync->folder->set({{QStringLiteral("SkippedCount"), QVariant::fromValue(count)}});
        }
        QTRY_COMPARE(sync->skippedCount(), 5003ULL);
        QTest::qWait(300);
        QCOMPARE(fake.sync->calls.count(QStringLiteral("Skipped")), asked);
        QTRY_COMPARE(fake.sync->calls.count(QStringLiteral("Skipped")), asked + 1);
        QTest::qWait(1200);
        QCOMPARE(fake.sync->calls.count(QStringLiteral("Skipped")), asked + 1);

        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("conflicts")));
        auto *conflicts = window->findChild<QQuickItem *>(QStringLiteral("conflictsList"));
        QVERIFY(conflicts);
        QTRY_COMPARE(conflicts->property("count").toInt(), 200);
        // Taller rows: about four in sight.
        QTRY_VERIFY(builtRows(conflicts, QStringLiteral("conflictRow")) >= 3);
        QTest::qWait(100);
        QVERIFY2(builtRows(conflicts, QStringLiteral("conflictRow")) <= 20, "more rows built than a few screens hold");
        QMetaObject::invokeMethod(conflicts, "positionViewAtEnd");
        auto *conflictsMore = window->findChild<QQuickItem *>(QStringLiteral("conflictsMore"));
        QVERIFY(conflictsMore);
        QTRY_VERIFY(conflictsMore->isVisible());
        QTest::qWait(100);
        QVERIFY2(builtRows(conflicts, QStringLiteral("conflictRow")) <= 20, "more rows built than a few screens hold");
        QCOMPARE(digits(conflictsMore->property("text").toString()), QStringLiteral("4800"));
        QVERIFY(conflictsMore->property("description").toString().contains(QStringLiteral("konedrivectl sync conflicts")));

        fake.stop();
    }
};

QTEST_MAIN(DialogsTest)

#include "dialogstest.moc"
