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

    /// The first visible form row (delegate) under `root`, by the visual tree,
    /// whose description holds `part`; null when none does.
    static QQuickItem *itemDescribed(const QQuickItem *root, const QString &part)
    {
        const auto children = root->childItems();
        for (QQuickItem *item : children) {
            if (!item->isVisible()) {
                continue;
            }
            if (QLatin1String(item->metaObject()->className()).contains(QLatin1String("Delegate"))
                && item->property("description").toString().contains(part)) {
                return item;
            }
            if (QQuickItem *found = itemDescribed(item, part)) {
                return found;
            }
        }
        return nullptr;
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
        TraySettings trayIcons;
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        ServiceUnit serviceUnit;
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places, &trayIcons, &serviceUnit);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);
        engine.loadFromModule("org.konedrive.app.window", "Main");
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
        QCOMPARE(remove->property("title").toString(), QStringLiteral("Disconnect Family?"));
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
        QCOMPARE(remove->property("title").toString(), QStringLiteral("Disconnect Personal?"));
        fake.removeAccount(personal);
        QTRY_VERIFY(!shown(remove));
        QTRY_COMPARE(remove->property("item").value<QObject *>(), nullptr);
        QMetaObject::invokeMethod(remove, "confirm");
        QTest::qWait(100);
        QVERIFY(!fake.manager->calls.contains(QStringLiteral("Remove:") + personal));

        fake.stop();
    }

    /// How the folder syncs: the mode line follows Mode. Two-way, chosen in the mode
    /// dialog, goes to the daemon only once confirmed; read-only while changes are unsent
    /// shows the switch that waits, with its two ways out.
    void theModeDialog()
    {
        FakeDaemon fake;
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        // The whole window is loaded here, and it opens a sign-in address in the
        // browser: the fake refuses the switch before any sign-in starts.
        fake.account->readWriteFails = QStringLiteral("cannot listen on localhost");
        fake.sync->folder->set({{QStringLiteral("Path"), QStringLiteral("/home/u/OneDrive")},
                                {QStringLiteral("State"), QStringLiteral("ready")},
                                {QStringLiteral("Source"), QStringLiteral("onedrive")}});
        QVERIFY(fake.start());

        Autostart autostart;
        DownloadProgressSettings progress;
        PlacesSettings places;
        TraySettings trayIcons;
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        ServiceUnit serviceUnit;
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places, &trayIcons, &serviceUnit);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);
        engine.loadFromModule("org.konedrive.app.window", "Main");
        QVERIFY(!engine.rootObjects().isEmpty());
        auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
        QVERIFY(window);
        window->show();
        QTRY_COMPARE(accounts.count(), 1);
        QTRY_VERIFY(accounts.at(0)->sync()->serviceAvailable() && accounts.at(0)->account()->state() == QLatin1String("signed-in"));
        QObject *dialog = window->findChild<QObject *>(QStringLiteral("modeDialog"));
        auto *modeLine = window->findChild<QQuickItem *>(QStringLiteral("modeLine"));
        QVERIFY(dialog && modeLine);
        QTRY_COMPARE(modeLine->property("text").toString(), QStringLiteral("Read-only"));

        // Two-way: chosen, then confirmed. The fake refuses it, so no browser opens here.
        QMetaObject::invokeMethod(window, "changeMode");
        QTRY_VERIFY(shown(dialog));
        QVERIFY(!dialog->property("twoWay").toBool());
        dialog->setProperty("twoWay", true);
        QVERIFY(!fake.account->calls.join(QLatin1Char(' ')).contains(QStringLiteral("SetMode")));
        QMetaObject::invokeMethod(dialog, "confirm");
        QTRY_VERIFY(fake.account->calls.contains(QStringLiteral("SetMode:read-write:no-force")));
        QTRY_COMPARE(accounts.at(0)->account()->actionError(), QStringLiteral("Two-way sync was not turned on: cannot listen on localhost"));
        QCOMPARE(modeLine->property("text").toString(), QStringLiteral("Read-only"));

        // It follows the mode the account runs in.
        fake.account->set({{QStringLiteral("Mode"), QStringLiteral("read-write")}});
        QTRY_COMPARE(modeLine->property("text").toString(), QStringLiteral("Two-way sync"));

        // Read-only, with nothing unsent: switched at once.
        QMetaObject::invokeMethod(window, "changeMode");
        QTRY_VERIFY(shown(dialog));
        QVERIFY(dialog->property("twoWay").toBool());
        dialog->setProperty("twoWay", false);
        QMetaObject::invokeMethod(dialog, "confirm");
        QTRY_VERIFY(fake.account->calls.contains(QStringLiteral("SetMode:read-only:no-force")));
        QTRY_COMPARE(accounts.at(0)->account()->mode(), QStringLiteral("read-only"));
        QTRY_VERIFY(accounts.at(0)->account()->switchingTo().isEmpty());
        QTRY_COMPARE(modeLine->property("text").toString(), QStringLiteral("Read-only"));

        // Read-only while changes are unsent: the mode line says that the switch waits.
        QObject *now = window->findChild<QObject *>(QStringLiteral("switchToReadOnlyNow"));
        QObject *cancel = window->findChild<QObject *>(QStringLiteral("cancelReadOnlySwitch"));
        QVERIFY(now && cancel);
        QVERIFY(!now->property("visible").toBool());
        fake.account->set({{QStringLiteral("Mode"), QStringLiteral("read-write")}});
        QTRY_COMPARE(modeLine->property("text").toString(), QStringLiteral("Two-way sync"));
        fake.account->unsentChanges = 2;
        QMetaObject::invokeMethod(window, "changeMode");
        dialog->setProperty("twoWay", false);
        QMetaObject::invokeMethod(dialog, "confirm");
        QTRY_COMPARE(modeLine->property("text").toString(), QStringLiteral("Switching to read-only: 2 changes still to send"));
        QVERIFY(now->property("visible").toBool() && cancel->property("visible").toBool());
        QTRY_VERIFY(accounts.at(0)->account()->switchingTo().isEmpty());
        QObject *accountError = window->findChild<QObject *>(QStringLiteral("accountError"));
        QVERIFY(accountError);
        QVERIFY(!accountError->property("visible").toBool());

        // Keep Two-Way Sync takes the wish back: read-write, and the state is gone.
        fake.account->calls.clear();
        QTRY_VERIFY(cancel->property("enabled").toBool());
        QMetaObject::invokeMethod(cancel, "clicked");
        QTRY_VERIFY(fake.account->calls.contains(QStringLiteral("SetMode:read-write:no-force")));
        QTRY_COMPARE(modeLine->property("text").toString(), QStringLiteral("Two-way sync"));

        // Switch Now forces it.
        QMetaObject::invokeMethod(window, "changeMode");
        dialog->setProperty("twoWay", false);
        QMetaObject::invokeMethod(dialog, "confirm");
        QTRY_VERIFY(now->property("visible").toBool());
        QTRY_VERIFY(now->property("enabled").toBool());
        QMetaObject::invokeMethod(now, "clicked");
        QTRY_VERIFY(fake.account->calls.contains(QStringLiteral("SetMode:read-only:force")));
        QTRY_COMPARE(accounts.at(0)->account()->mode(), QStringLiteral("read-only"));
        QTRY_COMPARE(modeLine->property("text").toString(), QStringLiteral("Read-only"));

        fake.stop();
    }

    /// Disconnecting and moving to another folder are never refused for what is unsent:
    /// the question says first how many changes stay on this device only. Another folder,
    /// confirmed, forgets this one and then opens the folder picker.
    void disconnectingAndAnotherFolderWarnOfUnsentChanges()
    {
        FakeDaemon fake;
        const QString root = QStringLiteral("/home/u/OneDrive");
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        fake.sync->folder->set({{QStringLiteral("Path"), root}, {QStringLiteral("State"), QStringLiteral("ready")}, {QStringLiteral("Source"), QStringLiteral("onedrive")}});
        QVERIFY(fake.start());

        Autostart autostart;
        DownloadProgressSettings progress;
        PlacesSettings places;
        TraySettings trayIcons;
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        ServiceUnit serviceUnit;
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places, &trayIcons, &serviceUnit);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);
        engine.loadFromModule("org.konedrive.app.window", "Main");
        QVERIFY(!engine.rootObjects().isEmpty());
        auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
        QVERIFY(window);
        window->show();
        QTRY_COMPARE(accounts.count(), 1);
        QTRY_VERIFY(accounts.at(0)->sync()->serviceAvailable() && accounts.at(0)->sync()->rootPath() == root);
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("oneDriveSettings")));
        QObject *remove = window->findChild<QObject *>(QStringLiteral("removeDialog"));
        QObject *another = window->findChild<QObject *>(QStringLiteral("anotherFolderDialog"));
        QVERIFY(remove && another);

        // Nothing unsent: the question says only what disconnecting does.
        const QString stops = QStringLiteral("KOneDrive stops syncing ") + root;
        QMetaObject::invokeMethod(remove, "open");
        QTRY_VERIFY(shown(remove));
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("UnsentCount")));
        QTest::qWait(100);
        QVERIFY(remove->property("subtitle").toString().startsWith(stops));
        QMetaObject::invokeMethod(remove, "close");
        QTRY_VERIFY(!shown(remove));

        // Two changes unsent: said first, in both questions.
        const QString warning = QStringLiteral("2 changes made here are not uploaded yet: they stay on this device only, as plain files.");
        fake.sync->uploadQueueRows = {{1, QStringLiteral("update"), root + QStringLiteral("/a.odt"), QStringLiteral("pending"), 0, 10, QString(), 0},
                                 {2, QStringLiteral("create"), root + QStringLiteral("/b.odt"), QStringLiteral("pending"), 0, 10, QString(), 0}};
        QMetaObject::invokeMethod(remove, "open");
        QTRY_VERIFY(shown(remove));
        QTRY_VERIFY(remove->property("subtitle").toString().startsWith(warning));
        QVERIFY(remove->property("subtitle").toString().contains(stops));
        QMetaObject::invokeMethod(remove, "close");
        QTRY_VERIFY(!shown(remove));
        QVERIFY(!fake.manager->calls.join(QLatin1Char(' ')).contains(QStringLiteral("Remove")));

        QMetaObject::invokeMethod(another, "open");
        QTRY_VERIFY(shown(another));
        QTRY_VERIFY(another->property("subtitle").toString().startsWith(warning));
        QVERIFY(another->property("subtitle").toString().contains(stops));
        // Declined: nothing is forgotten.
        QMetaObject::invokeMethod(another, "close");
        QTRY_VERIFY(!shown(another));
        QTest::qWait(100);
        QVERIFY(!fake.sync->calls.contains(QStringLiteral("Unregister")));
        // Asked again and confirmed: forgotten, then the picker opens for the account.
        QMetaObject::invokeMethod(another, "open");
        QTRY_VERIFY(shown(another));
        QMetaObject::invokeMethod(another, "confirm");
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("Unregister")));
        QTRY_VERIFY(!shown(another));
        QTRY_VERIFY(accounts.at(0)->sync()->rootPath().isEmpty());
        QObject *picker = window->findChild<QObject *>(QStringLiteral("folderDialog"));
        QVERIFY(picker);
        QTRY_COMPARE(picker->property("item").value<QObject *>(), accounts.at(0));

        fake.stop();
    }

    /// Issue #80: OneDrive Settings' previews switch shows what the daemon says and sets
    /// it; the line about Dolphin's previews shows while it is off.
    void theThumbnailsSetting()
    {
        FakeDaemon fake;
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        // The whole window is loaded here, and it opens a sign-in address in the
        // browser: the fake refuses the switch before any sign-in starts.
        fake.account->readWriteFails = QStringLiteral("cannot listen on localhost");
        fake.sync->folder->set({{QStringLiteral("Path"), QStringLiteral("/home/u/OneDrive")},
                                {QStringLiteral("State"), QStringLiteral("ready")},
                                {QStringLiteral("Source"), QStringLiteral("onedrive")}});
        QVERIFY(fake.start());

        Autostart autostart;
        DownloadProgressSettings progress;
        PlacesSettings places;
        TraySettings trayIcons;
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        ServiceUnit serviceUnit;
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places, &trayIcons, &serviceUnit);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);
        engine.loadFromModule("org.konedrive.app.window", "Main");
        QVERIFY(!engine.rootObjects().isEmpty());
        auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
        QVERIFY(window);
        window->show();
        QTRY_COMPARE(accounts.count(), 1);
        QTRY_VERIFY(accounts.at(0)->sync()->serviceAvailable() && accounts.at(0)->account()->state() == QLatin1String("signed-in"));
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("oneDriveSettings")));
        auto *thumbnails = window->findChild<QQuickItem *>(QStringLiteral("thumbnailsSwitch"));
        auto *offLine = window->findChild<QQuickItem *>(QStringLiteral("thumbnailsOffLine"));
        QVERIFY(thumbnails && offLine);
        QTRY_VERIFY(thumbnails->isVisible());
        QVERIFY(thumbnails->property("checked").toBool());
        QVERIFY(!offLine->isVisible());

        // A switch turned by the user asks the daemon; the daemon's answer is what shows.
        thumbnails->setProperty("checked", false);
        QMetaObject::invokeMethod(thumbnails, "toggled");
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("SetThumbnails:off")));
        QTRY_VERIFY(!thumbnails->property("checked").toBool());
        QTRY_VERIFY(offLine->isVisible());

        // Changed elsewhere (konedrivectl), the page follows.
        fake.sync->folder->set({{QStringLiteral("Thumbnails"), true}});
        QTRY_VERIFY(thumbnails->property("checked").toBool());
        QTRY_VERIFY(!offLine->isVisible());

        // Issue #217: the free space to keep, in gigabytes, shows what the daemon says
        // and is set through it.
        auto *freeSpace = window->findChild<QQuickItem *>(QStringLiteral("freeSpaceReserveBox"));
        QVERIFY(freeSpace);
        QTRY_COMPARE(freeSpace->property("value").toInt(), 10);
        freeSpace->setProperty("value", 25);
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("SetFreeSpaceReserve:25")));
        fake.sync->folder->set({{QStringLiteral("FreeSpaceReserve"), qulonglong(3)}});
        QTRY_COMPARE(freeSpace->property("value").toInt(), 3);
        QCOMPARE(fake.sync->calls.count(QStringLiteral("SetFreeSpaceReserve:3")), 0);
        fake.stop();
    }

    /// Issue #95: pausing on metered connections and on battery is the whole app's, on the
    /// Settings page; the controls show what the manager says and set it there.
    void theHoldSettings()
    {
        FakeDaemon fake;
        QVERIFY(fake.start());

        Autostart autostart;
        DownloadProgressSettings progress;
        PlacesSettings places;
        TraySettings trayIcons;
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        ServiceUnit serviceUnit;
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places, &trayIcons, &serviceUnit);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);
        engine.loadFromModule("org.konedrive.app.window", "Main");
        QVERIFY(!engine.rootObjects().isEmpty());
        auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
        QVERIFY(window);
        window->show();
        QTRY_VERIFY(daemon.serviceAvailable());
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("settings")));
        QQuickItem *metered = nullptr;
        QQuickItem *battery = nullptr;
        QTRY_VERIFY((metered = window->findChild<QQuickItem *>(QStringLiteral("meteredSwitch"))));
        QTRY_VERIFY((battery = window->findChild<QQuickItem *>(QStringLiteral("batteryCombo"))));
        QTRY_VERIFY(metered->isVisible() && metered->isEnabled());
        QVERIFY(metered->property("checked").toBool());
        QCOMPARE(battery->property("currentIndex").toInt(), 1);

        // Turned by the user, the manager is asked; its answer is what shows.
        metered->setProperty("checked", false);
        QMetaObject::invokeMethod(metered, "toggled");
        QTRY_VERIFY(fake.manager->calls.contains(QStringLiteral("SetPauseOnMetered:off")));
        QTRY_VERIFY(!metered->property("checked").toBool());
        QMetaObject::invokeMethod(battery, "activated", Q_ARG(int, 2));
        QTRY_VERIFY(fake.manager->calls.contains(QStringLiteral("SetOnBattery:pause")));
        QTRY_COMPARE(battery->property("currentIndex").toInt(), 2);

        // Changed elsewhere (konedrivectl settings), the page follows.
        fake.manager->set({{QStringLiteral("PauseOnMetered"), true}, {QStringLiteral("OnBattery"), QStringLiteral("sync")}});
        QTRY_VERIFY(metered->property("checked").toBool());
        QTRY_COMPARE(battery->property("currentIndex").toInt(), 0);

        // With the daemon gone, the controls cannot be used.
        fake.stop();
        QTRY_VERIFY(!daemon.serviceAvailable());
        QTRY_VERIFY(!metered->isEnabled());
        QVERIFY(!battery->isEnabled());
    }

    /// Settings' About names this build; a daemon of another build (installed, not
    /// restarted) adds a second line, which goes once the daemon runs this build.
    void theVersionLine()
    {
        FakeDaemon fake;
        QVERIFY(fake.start());

        Autostart autostart;
        DownloadProgressSettings progress;
        PlacesSettings places;
        TraySettings trayIcons;
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        ServiceUnit serviceUnit;
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places, &trayIcons, &serviceUnit);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);
        engine.loadFromModule("org.konedrive.app.window", "Main");
        QVERIFY(!engine.rootObjects().isEmpty());
        auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
        QVERIFY(window);
        window->show();
        QTRY_VERIFY(daemon.serviceAvailable());
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("settings")));

        QQuickItem *line = nullptr;
        QQuickItem *service = nullptr;
        QTRY_VERIFY((line = window->findChild<QQuickItem *>(QStringLiteral("versionLine"))));
        QTRY_VERIFY((service = window->findChild<QQuickItem *>(QStringLiteral("serviceVersionLine"))));
        const QString commit = QStringLiteral(KONEDRIVE_COMMIT);
        QCOMPARE(line->property("text").toString(),
                 QStringLiteral("Version %1 · commit %2").arg(QStringLiteral(KONEDRIVE_VERSION), commit.left(7)));
        QVERIFY(line->isVisible());
        // The same build: no second line.
        QVERIFY(!service->isVisible());

        // Another build comes up (installed, not restarted before).
        fake.stop();
        QTRY_VERIFY(!daemon.serviceAvailable());
        QVERIFY(!service->isVisible());
        fake.manager->setBuild(QStringLiteral("0.0.9-dev.55"), QStringLiteral("1a2b3c4d5e6f1a2b3c4d5e6f1a2b3c4d5e6f1a2b"));
        QVERIFY(fake.start());
        QTRY_VERIFY(service->isVisible());
        QCOMPARE(service->property("text").toString(),
                 QStringLiteral("Service: 0.0.9-dev.55 · commit 1a2b3c4 — restart it to use this version"));

        // The same version from another commit differs too.
        fake.stop();
        QTRY_VERIFY(!service->isVisible());
        fake.manager->setBuild(QStringLiteral(KONEDRIVE_VERSION), QStringLiteral("1a2b3c4d5e6f1a2b3c4d5e6f1a2b3c4d5e6f1a2b"));
        QVERIFY(fake.start());
        QTRY_VERIFY(service->isVisible());

        // Restarted on this build: the line goes.
        fake.stop();
        QTRY_VERIFY(!daemon.serviceAvailable());
        fake.manager->setBuild(QStringLiteral(KONEDRIVE_VERSION), commit);
        QVERIFY(fake.start());
        QTRY_VERIFY(daemon.serviceAvailable());
        QVERIFY(!service->isVisible());
        QCOMPARE(line->property("text").toString(),
                 QStringLiteral("Version %1 · commit %2").arg(QStringLiteral(KONEDRIVE_VERSION), commit.left(7)));
    }

    /// Issue #57: while the account holds back by itself, the head of the Overview says why,
    /// with a Sync Anyway button.
    void theOverviewShowsAHold()
    {
        FakeDaemon fake;
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        fake.sync->folder->set({{QStringLiteral("Path"), QStringLiteral("/home/u/OneDrive")},
                                {QStringLiteral("State"), QStringLiteral("ready")},
                                {QStringLiteral("Source"), QStringLiteral("onedrive")},
                                {QStringLiteral("HeldBack"), QStringLiteral("metered")}});
        fake.sync->folder->decide("paused", "held-back");
        QVERIFY(fake.start());

        Autostart autostart;
        DownloadProgressSettings progress;
        PlacesSettings places;
        TraySettings trayIcons;
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        ServiceUnit serviceUnit;
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places, &trayIcons, &serviceUnit);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);
        engine.loadFromModule("org.konedrive.app.window", "Main");
        QVERIFY(!engine.rootObjects().isEmpty());
        auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
        QVERIFY(window);
        window->show();
        QTRY_COMPARE(accounts.count(), 1);
        QTRY_VERIFY(accounts.at(0)->sync()->serviceAvailable() && accounts.at(0)->account()->state() == QLatin1String("signed-in"));
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("overview")));
        auto *headline = window->findChild<QQuickItem *>(QStringLiteral("headline"));
        auto *anyway = window->findChild<QQuickItem *>(QStringLiteral("syncAnywayButton"));
        auto *resume = window->findChild<QQuickItem *>(QStringLiteral("resumeButton"));
        QVERIFY(headline && anyway && resume);
        QTRY_COMPARE(headline->property("text").toString(), QStringLiteral("Paused: metered connection"));
        QVERIFY(anyway->isVisible());
        QVERIFY(!resume->isVisible());

        fake.sync->folder->set({{QStringLiteral("HeldBack"), QStringLiteral("power-saver")}});
        QTRY_COMPARE(headline->property("text").toString(), QStringLiteral("Paused: power-saver mode"));
        QMetaObject::invokeMethod(anyway, "clicked");
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("SyncAnyway")));
        fake.sync->folder->decide("ok", "up-to-date", {{QStringLiteral("HeldBack"), QString()}});
        QTRY_COMPARE(headline->property("text").toString(), QStringLiteral("Up to date"));
        QVERIFY(!anyway->isVisible());
        fake.stop();
    }

    /// Needs Your Attention shows while something needs the user, with its line, and goes
    /// once nothing does.
    void theOverviewShowsWhatNeedsAttention()
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
        TraySettings trayIcons;
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        ServiceUnit serviceUnit;
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places, &trayIcons, &serviceUnit);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);
        engine.loadFromModule("org.konedrive.app.window", "Main");
        QVERIFY(!engine.rootObjects().isEmpty());
        auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
        QVERIFY(window);
        window->show();
        QTRY_COMPARE(accounts.count(), 1);
        QTRY_VERIFY(accounts.at(0)->sync()->serviceAvailable() && accounts.at(0)->account()->state() == QLatin1String("signed-in"));
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("overview")));
        auto *headline = window->findChild<QQuickItem *>(QStringLiteral("headline"));
        auto *attention = window->findChild<QQuickItem *>(QStringLiteral("attentionCard"));
        auto *conflictsLine = window->findChild<QQuickItem *>(QStringLiteral("conflictsLine"));
        QVERIFY(headline && attention && conflictsLine);
        QTRY_VERIFY(headline->isVisible());
        QVERIFY(!attention->isVisible());

        fake.sync->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(2)}});
        QTRY_VERIFY(attention->isVisible() && conflictsLine->isVisible());
        fake.sync->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(0)}});
        QTRY_VERIFY(!attention->isVisible());
        fake.stop();
    }

    /// The Overview says how the folder syncs — "Read-only for now" while a two-way
    /// folder cannot be watched (Folder.Writable) — and, while the check of the local files
    /// runs, since when and why.
    void theOverviewShowsTheModeAndTheLocalScan()
    {
        FakeDaemon fake;
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}, {QStringLiteral("Mode"), QStringLiteral("read-write")}});
        fake.sync->folder->set({{QStringLiteral("Path"), QStringLiteral("/home/u/OneDrive")},
                                {QStringLiteral("State"), QStringLiteral("ready")},
                                {QStringLiteral("Source"), QStringLiteral("onedrive")}});
        fake.sync->scan->set({{QStringLiteral("State"), QStringLiteral("running")},
                              {QStringLiteral("Reason"), QStringLiteral("start")},
                              {QStringLiteral("Started"), QVariant::fromValue<qlonglong>(QDateTime::currentSecsSinceEpoch() - 125)}});
        QVERIFY(fake.start());

        Autostart autostart;
        DownloadProgressSettings progress;
        PlacesSettings places;
        TraySettings trayIcons;
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        ServiceUnit serviceUnit;
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places, &trayIcons, &serviceUnit);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);
        engine.loadFromModule("org.konedrive.app.window", "Main");
        QVERIFY(!engine.rootObjects().isEmpty());
        auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
        QVERIFY(window);
        window->show();
        QTRY_COMPARE(accounts.count(), 1);
        QTRY_VERIFY(accounts.at(0)->sync()->serviceAvailable() && accounts.at(0)->account()->state() == QLatin1String("signed-in"));
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("overview")));
        auto *modeLine = window->findChild<QQuickItem *>(QStringLiteral("modeLine"));
        QVERIFY(modeLine);
        QTRY_COMPARE(modeLine->property("text").toString(), QStringLiteral("Two-way sync"));
        fake.sync->folder->set({{QStringLiteral("Writable"), false}});
        QTRY_COMPARE(modeLine->property("text").toString(), QStringLiteral("Read-only for now"));
        fake.sync->folder->set({{QStringLiteral("Writable"), true}});
        QTRY_COMPARE(modeLine->property("text").toString(), QStringLiteral("Two-way sync"));
        auto *line = window->findChild<QQuickItem *>(QStringLiteral("scanLine"));
        QVERIFY(line);
        QTRY_VERIFY(line->isVisible());
        QCOMPARE(line->property("text").toString(),
                 QStringLiteral("Checking local files — started 2 min ago, as syncing started"));

        // A check that ended says nothing: what changes after it is seen as it is made.
        fake.sync->scan->set({{QStringLiteral("State"), QStringLiteral("idle")},
                              {QStringLiteral("Finished"), QVariant::fromValue<qlonglong>(QDateTime::currentSecsSinceEpoch() - 300)},
                              {QStringLiteral("Took"), QVariant::fromValue<uint>(40)}});
        QTRY_VERIFY(!line->isVisible());
        fake.stop();
    }

    /// Setting up an account that has no folder: the Overview offers ~/OneDrive, made when
    /// it is not there, and then asks how the folder syncs. HOME is a temporary directory.
    void theOverviewSetsUpAFolderAndAMode()
    {
        QTemporaryDir home;
        QVERIFY(home.isValid());
        const QByteArray realHome = qgetenv("HOME");
        qputenv("HOME", QFile::encodeName(home.path()));
        FakeDaemon fake;
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        fake.sync->folder->decide("offline", "no-folder");
        QVERIFY(fake.start());

        Autostart autostart;
        DownloadProgressSettings progress;
        PlacesSettings places;
        TraySettings trayIcons;
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        ServiceUnit serviceUnit;
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places, &trayIcons, &serviceUnit);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);
        engine.loadFromModule("org.konedrive.app.window", "Main");
        QVERIFY(!engine.rootObjects().isEmpty());
        auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
        QVERIFY(window);
        window->show();
        QTRY_COMPARE(accounts.count(), 1);
        QTRY_VERIFY(accounts.at(0)->sync()->serviceAvailable() && accounts.at(0)->account()->state() == QLatin1String("signed-in"));
        auto *headline = window->findChild<QQuickItem *>(QStringLiteral("headline"));
        auto *useDefault = window->findChild<QQuickItem *>(QStringLiteral("useDefaultFolderButton"));
        auto *modeChoice = window->findChild<QQuickItem *>(QStringLiteral("modeChoiceCard"));
        auto *readOnly = window->findChild<QQuickItem *>(QStringLiteral("chooseReadOnly"));
        auto *modeLine = window->findChild<QQuickItem *>(QStringLiteral("modeLine"));
        QVERIFY(headline && useDefault && modeChoice && readOnly && modeLine);
        QTRY_COMPARE(headline->property("text").toString(), QStringLiteral("Choose where your OneDrive appears"));
        QTRY_VERIFY(useDefault->isVisible());
        QCOMPARE(useDefault->property("text").toString(), QStringLiteral("Use ~/OneDrive"));

        QMetaObject::invokeMethod(useDefault, "clicked");
        const QString folder = home.path() + QStringLiteral("/OneDrive");
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("Register:") + folder));
        QVERIFY(QFileInfo(folder).isDir());
        fake.sync->folder->decide("ok", "up-to-date");
        QTRY_VERIFY(modeChoice->isVisible());
        QVERIFY(!modeLine->isVisible());

        QMetaObject::invokeMethod(readOnly, "clicked");
        QTRY_VERIFY(!modeChoice->isVisible());
        QTRY_VERIFY(modeLine->isVisible());
        QCOMPARE(modeLine->property("text").toString(), QStringLiteral("Read-only"));
        QVERIFY(!fake.account->calls.join(QLatin1Char(' ')).contains(QStringLiteral("SetMode")));
        fake.stop();
        qputenv("HOME", realHome);
    }

    /// With no service, the Overview says so and starts it; a start refused (the test bus
    /// has no systemd) says it could not, and offers to start again.
    void theOverviewStartsTheService()
    {
        Autostart autostart;
        DownloadProgressSettings progress;
        PlacesSettings places;
        TraySettings trayIcons;
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        ServiceUnit serviceUnit;
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places, &trayIcons, &serviceUnit);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);
        engine.loadFromModule("org.konedrive.app.window", "Main");
        QVERIFY(!engine.rootObjects().isEmpty());
        auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
        QVERIFY(window);
        window->show();
        auto *card = window->findChild<QQuickItem *>(QStringLiteral("serviceCard"));
        auto *start = window->findChild<QQuickItem *>(QStringLiteral("startServiceButton"));
        QVERIFY(card && start);
        QTRY_VERIFY(card->isVisible());
        QCOMPARE(start->property("text").toString(), QStringLiteral("Start Service"));
        QMetaObject::invokeMethod(start, "clicked");
        QTRY_VERIFY(!serviceUnit.startError().isEmpty());
        QTRY_COMPARE(start->property("text").toString(), QStringLiteral("Start Again"));
        QVERIFY(card->isVisible());
    }

    /// Issue #20: with 5000 changes OneDrive has no room for and 3 names it
    /// refuses, Issues and Activity come up at once and build a handful of rows
    /// — a line per reason, files only for a per-file reason opened — and the
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
                               {QStringLiteral("BlockedCount"), QVariant::fromValue<uint>(5005)}});
        fake.sync->transfers->set({{QStringLiteral("DownloadLeftCount"), QVariant::fromValue<uint>(1234)},
                                   {QStringLiteral("DownloadLeftBytes"), QVariant::fromValue<qulonglong>(3ULL << 30)},
                                   {QStringLiteral("DownloadDoneBytes"), QVariant::fromValue<qulonglong>(1ULL << 30)},
                                   {QStringLiteral("DownloadTimeLeft"), QVariant::fromValue<uint>(720)},
                                   {QStringLiteral("UploadLeftCount"), QVariant::fromValue<uint>(2)},
                                   {QStringLiteral("UploadLeftBytes"), QVariant::fromValue<qulonglong>(1024)},
                                   {QStringLiteral("UploadDoneBytes"), QVariant::fromValue<qulonglong>(0)},
                                   {QStringLiteral("UploadTimeLeft"), QVariant::fromValue<uint>(0)}});
        fake.sync->keptBack = {{QStringLiteral("one-action"), QStringLiteral("quota-exceeded"), 5000, 5000ULL << 20},
                               {QStringLiteral("one-action"), QStringLiteral("forbidden"), 2, 2048},
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
        TraySettings trayIcons;
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        ServiceUnit serviceUnit;
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places, &trayIcons, &serviceUnit);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);
        engine.loadFromModule("org.konedrive.app.window", "Main");
        QVERIFY(!engine.rootObjects().isEmpty());
        auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
        QVERIFY(window);
        window->show();
        QTRY_COMPARE(accounts.count(), 1);
        QTRY_VERIFY(accounts.at(0)->sync()->serviceAvailable() && accounts.at(0)->sync()->blockedCount() == 5005);
        SyncController *sync = accounts.at(0)->sync();

        QElapsedTimer clock;
        clock.start();
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("issues")));
        auto *page = window->findChild<QQuickItem *>(QStringLiteral("issuesPage"));
        QVERIFY(page);
        QTRY_VERIFY(sync->notUploadedKnown());
        QTRY_COMPARE(itemsSaying(page, QStringLiteral("changes: OneDrive is full")), 1);
        QVERIFY2(clock.elapsed() < 3000, qPrintable(QString::number(clock.elapsed())));
        QCOMPARE(itemsSaying(page, QStringLiteral("3 files")), 1);
        QCOMPARE(itemsSaying(page, root), 0);
        QCOMPARE(itemsSaying(page, QStringLiteral("Check Again")), 2);
        QCOMPARE(itemsSaying(page, QStringLiteral("Open OneDrive on the Web")), 1);
        const int rows = itemsSaying(page, QString());
        QVERIFY2(rows <= 10, qPrintable(QString::number(rows)));

        // What OneDrive did not allow is checked again by a refresh; no sign-in is offered for it.
        QCOMPARE(itemsSaying(page, QStringLiteral("changes: OneDrive does not allow this change")), 1);
        QCOMPARE(itemsSaying(page, QStringLiteral("Sign In Again")), 0);
        QQuickItem *tryAgain = itemDescribed(page, QStringLiteral("If OneDrive allows these changes now, they go up."));
        QVERIFY(tryAgain);
        QCOMPARE(tryAgain->property("text").toString(), QStringLiteral("Check Again"));
        QVERIFY(!fake.sync->calls.contains(QStringLiteral("Refresh")));
        QMetaObject::invokeMethod(tryAgain, "clicked");
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("Refresh")));

        // Opened, the refused names are listed; the full OneDrive never is.
        QMetaObject::invokeMethod(page, "setOpened", Q_ARG(QVariant, QStringLiteral("name-characters")), Q_ARG(QVariant, true));
        QTRY_COMPARE(itemsSaying(page, root), 3);
        QVERIFY(fake.sync->calls.contains(QStringLiteral("NotUploadedFiles:name-characters:20")));
        QVERIFY(!fake.sync->calls.join(QLatin1Char(' ')).contains(QStringLiteral("NotUploadedFiles:quota-exceeded")));

        // Activity: what is left is in the cards (issue #16), and one line leads to what can't upload.
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("activity")));
        auto *activity = window->findChild<QQuickItem *>(QStringLiteral("activityPage"));
        QVERIFY(activity);
        auto *link = window->findChild<QQuickItem *>(QStringLiteral("keptBackLink"));
        QVERIFY(link);
        QTRY_VERIFY(link->isVisible());
        QTRY_VERIFY2(link->property("text").toString().endsWith(QStringLiteral("005 changes can't upload")),
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

        // The thumbnails' line is always there, so that nothing moves when they start or end.
        QCOMPARE(text("thumbnailDownloads"), QStringLiteral("0 thumbnails downloading"));
        fake.sync->transfers->set({{QStringLiteral("ThumbnailDownloads"), QVariant::fromValue<uint>(2)}});
        QTRY_COMPARE(text("thumbnailDownloads"), QStringLiteral("2 thumbnails downloading"));

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
            fake.sync->skippedList << KonedriveNotInFolderItem{root + QStringLiteral("/Shared/%1").arg(i), QStringLiteral("shared"), QString(), QString()};
            fake.sync->conflictList << KonedriveConflict{i, root + QStringLiteral("/%1.txt").arg(i), root + QStringLiteral("/%1-fedora.txt").arg(i), QStringLiteral("copy")};
        }
        QVERIFY(fake.start());

        Autostart autostart;
        DownloadProgressSettings progress;
        PlacesSettings places;
        TraySettings trayIcons;
        DaemonController daemon;
        AccountsModel accounts(&daemon);
        CurrentAccount current(&accounts);
        ServiceUnit serviceUnit;
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places, &trayIcons, &serviceUnit);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);
        engine.loadFromModule("org.konedrive.app.window", "Main");
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
