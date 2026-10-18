#include "fakedaemon.h"
#include "qmlregistration.h"

#include <KLocalizedQmlContext>
#include <KLocalizedString>

#include <QDateTime>
#include <QDesktopServices>
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

/// The app's objects and its window: Main.qml loaded as main() loads it.
struct App {
    Autostart autostart;
    DownloadProgressSettings progress;
    PlacesSettings places;
    TraySettings trayIcons;
    DaemonController daemon;
    AccountsModel accounts{&daemon};
    CurrentAccount current{&accounts};
    ServiceUnit serviceUnit;
    QQmlApplicationEngine engine;
    QQuickWindow *window = nullptr;

    /// Loads and shows the window; false when it could not be loaded.
    bool load()
    {
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places, &trayIcons, &serviceUnit);
        KLocalization::setupLocalizedContext(&engine);
        engine.loadFromModule("org.konedrive.app.window", "Main");
        window = engine.rootObjects().isEmpty() ? nullptr : qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
        if (window) {
            window->show();
        }
        return window != nullptr;
    }

    template<typename T = QQuickItem>
    T *find(const char *name) const
    {
        return window->findChild<T *>(QLatin1String(name));
    }

    /// What is called `name` on the page called `page`: for what two pages have.
    template<typename T = QQuickItem>
    T *findOn(const char *page, const char *name) const
    {
        auto *found = window->findChild<QObject *>(QLatin1String(page));
        return found ? found->findChild<T *>(QLatin1String(name)) : nullptr;
    }

    /// Whether the sidebar shows the entry of the page `name`; the entries are a
    /// Repeater's, found by the visual tree.
    bool inSidebar(const QString &name) const
    {
        const QQuickItem *entry = named(window->contentItem(), QStringLiteral("sidebar-") + name);
        return entry && entry->isVisible();
    }

    static const QQuickItem *named(const QQuickItem *root, const QString &name)
    {
        const auto children = root->childItems();
        for (const QQuickItem *item : children) {
            if (item->objectName() == name) {
                return item;
            }
            if (const QQuickItem *found = named(item, name)) {
                return found;
            }
        }
        return nullptr;
    }

    /// The page the window shows: its objectName.
    QString pageShown() const
    {
        auto *stack = window->property("pageStack").value<QObject *>();
        auto *page = stack ? stack->property("currentItem").value<QObject *>() : nullptr;
        return page ? page->objectName() : QString();
    }
};

/// The window's pages, by what the account and its folder are, and its confirmations,
/// which act on the account they were opened for: another account chosen while one is
/// open — by a notification, the tray, or konedrivectl removing the one shown — closes it
/// and never redirects it. Main.qml as the app loads it, offscreen, against the fake daemon;
/// XDG_CONFIG_HOME is a temporary directory, and an address the window opens is kept here
/// rather than given to a browser.
class DialogsTest : public QObject
{
    Q_OBJECT

public Q_SLOTS:
    /// QDesktopServices' handler of https: what Qt.openUrlExternally was given.
    void openUrl(const QUrl &url)
    {
        m_opened << url;
    }

private:
    QTemporaryDir m_config;
    QList<QUrl> m_opened;

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
        QDesktopServices::setUrlHandler(QStringLiteral("https"), this, "openUrl");
    }

    void init()
    {
        m_opened.clear();
    }

    /// Adding a OneDrive is one page, the first one and one more alike: Sign In, its wait with
    /// the link to copy and Cancel, and why a sign-in added nothing. Added, the account's
    /// Status offers its folder; the pages about the folder show only once it has one.
    void addingAOneDriveIsOnePage()
    {
        FakeDaemon fake({});
        QVERIFY(fake.start());
        App app;
        QVERIFY(app.load());
        QTRY_VERIFY(app.daemon.serviceAvailable());
        QTRY_COMPARE(app.pageShown(), QStringLiteral("signInPage"));
        auto *signIn = app.find("signInButton");
        auto *wait = app.find("signInWait");
        auto *copy = app.find("copySignInLink");
        auto *error = app.find("addError");
        QVERIFY(signIn && wait && copy && error);
        QVERIFY(signIn->isVisible() && !wait->isVisible() && !error->isVisible());
        // With no account, only adding one and Settings are in the sidebar.
        QVERIFY(app.inSidebar(QStringLiteral("signIn")));
        QVERIFY(!app.inSidebar(QStringLiteral("status")));
        QVERIFY(!app.inSidebar(QStringLiteral("issues")));
        QVERIFY(app.inSidebar(QStringLiteral("settings")));

        // Refused: why, and Sign In again.
        fake.manager->refuseSignIn = QStringLiteral("no client id");
        QMetaObject::invokeMethod(signIn, "clicked");
        QTRY_VERIFY(error->isVisible());
        QCOMPARE(error->property("text").toString(), QStringLiteral("no client id"));
        QVERIFY(signIn->isVisible());

        // The sign-in waits in the browser: its link can be copied.
        fake.manager->refuseSignIn.clear();
        QMetaObject::invokeMethod(signIn, "clicked");
        QTRY_VERIFY(wait->isVisible());
        QVERIFY(!error->isVisible() && !signIn->isVisible());
        QTRY_COMPARE(m_opened.size(), 1);
        QVERIFY(copy->isEnabled());
        fake.finishSignIn(QStringLiteral("signed-in"), QStringLiteral("ann@example.com"));
        QTRY_COMPARE(app.accounts.count(), 1);
        QTRY_COMPARE(app.pageShown(), QStringLiteral("statusPage"));
        QCOMPARE(app.current.path(), fake.object(0)->path);
        QTRY_VERIFY(app.find("folderChoiceCard")->isVisible());
        QVERIFY(!app.find("headCard")->isVisible());
        QVERIFY(!app.inSidebar(QStringLiteral("signIn")));
        QVERIFY(app.inSidebar(QStringLiteral("status")));
        QVERIFY(!app.inSidebar(QStringLiteral("issues")) && !app.inSidebar(QStringLiteral("storage")) && !app.inSidebar(QStringLiteral("oneDriveSettings")));
        // A page about the folder gives way to Status.
        QMetaObject::invokeMethod(app.window, "showPage", Q_ARG(QVariant, QStringLiteral("activity")));
        QCOMPARE(app.pageShown(), QStringLiteral("statusPage"));

        // One more, from the switcher: the same page, and Cancel gives the sign-in up.
        auto *switcher = app.find<QObject>("accountSwitcher");
        QVERIFY(switcher);
        QMetaObject::invokeMethod(switcher, "addRequested");
        QTRY_COMPARE(app.pageShown(), QStringLiteral("signInPage"));
        QVERIFY(signIn->isVisible());
        QMetaObject::invokeMethod(signIn, "clicked");
        QTRY_VERIFY(wait->isVisible());
        QMetaObject::invokeMethod(app.find("cancelSignIn"), "clicked");
        QTRY_VERIFY(signIn->isVisible());
        QVERIFY(!error->isVisible());
        QCOMPARE(app.accounts.count(), 1);
        fake.stop();
    }

    /// The account with no folder: its Status is the folder step, which offers ~/OneDrive (or
    /// the next free one); Continue makes and registers it. Two-way waits for the browser
    /// first. Cancel asks to disconnect the account. HOME is a temporary directory.
    void theFolderStepGivesTheFolderOffered()
    {
        QTemporaryDir home;
        QVERIFY(home.isValid());
        const QByteArray realHome = qgetenv("HOME");
        qputenv("HOME", QFile::encodeName(home.path()));
        FakeDaemon fake({QStringLiteral("ann@example.com"), QStringLiteral("Family")});
        fake.object(0)->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}, {QStringLiteral("Email"), QStringLiteral("ann@example.com")}});
        fake.object(0)->sync->folder->decide("offline", "no-folder");
        fake.object(1)->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        fake.object(1)->sync->folder->set({{QStringLiteral("Path"), QString(home.path() + QStringLiteral("/OneDrive"))},
                                           {QStringLiteral("State"), QStringLiteral("ready")},
                                           {QStringLiteral("Source"), QStringLiteral("onedrive")}});
        QVERIFY(fake.start());
        App app;
        QVERIFY(app.load());
        QTRY_COMPARE(app.accounts.count(), 2);
        app.current.select(fake.object(0)->path);
        QTRY_VERIFY(app.accounts.at(0)->account()->state() == QLatin1String("signed-in"));
        auto *card = app.find("folderChoiceCard");
        auto *offered = app.find("offeredFolder");
        auto *twoWay = app.find("twoWaySwitch");
        auto *next = app.find("continueButton");
        QVERIFY(card && offered && twoWay && next);
        QTRY_VERIFY(card->isVisible());
        // Family has ~/OneDrive.
        QTRY_COMPARE(offered->property("text").toString(), QStringLiteral("~/OneDrive-ann"));
        QVERIFY(!twoWay->property("checked").toBool());

        // Two-way: the switch first, the folder once it is granted.
        twoWay->setProperty("checked", true);
        QMetaObject::invokeMethod(twoWay, "toggled");
        QMetaObject::invokeMethod(next, "clicked");
        QTRY_VERIFY(fake.object(0)->account->calls.contains(QStringLiteral("SetMode:read-write:no-force")));
        QTRY_COMPARE(m_opened.size(), 1);
        QTRY_VERIFY(app.find("cancelTwoWayButton")->isVisible());
        QVERIFY(!next->isVisible());
        QVERIFY(!fake.object(0)->sync->calls.join(QLatin1Char(' ')).contains(QStringLiteral("Register")));
        fake.object(0)->account->grantReadWrite();
        const QString folder = home.path() + QStringLiteral("/OneDrive-ann");
        QTRY_VERIFY(fake.object(0)->sync->calls.contains(QStringLiteral("Register:") + folder));
        QVERIFY(QFileInfo(folder).isDir());
        fake.object(0)->sync->folder->decide("ok", "up-to-date");
        QTRY_VERIFY(!card->isVisible());
        QTRY_VERIFY(app.find("headCard")->isVisible());
        QVERIFY(app.inSidebar(QStringLiteral("issues")) && app.inSidebar(QStringLiteral("storage")));

        // Family's folder forgotten: the step again, offering ~/OneDrive; Cancel asks to disconnect.
        const QString family = fake.object(1)->path;
        app.current.select(family);
        fake.object(1)->sync->folder->set({{QStringLiteral("Path"), QString()}, {QStringLiteral("State"), QStringLiteral("none")}});
        fake.object(1)->sync->folder->decide("offline", "no-folder");
        QTRY_VERIFY(card->isVisible());
        QTRY_COMPARE(offered->property("text").toString(), QStringLiteral("~/OneDrive"));
        auto *cancel = app.find<QObject>("cancelSetUpDialog");
        QVERIFY(cancel);
        QMetaObject::invokeMethod(app.find("cancelSetUpButton"), "clicked");
        QTRY_VERIFY(shown(cancel));
        QCOMPARE(cancel->property("title").toString(), QStringLiteral("Disconnect Family?"));
        QMetaObject::invokeMethod(cancel, "confirm");
        QTRY_VERIFY(fake.manager->calls.contains(QStringLiteral("Remove:") + family));
        // Closed, not closing: its transition must not outlive the window.
        QTRY_VERIFY(!shown(cancel));
        QTRY_COMPARE(app.accounts.count(), 1);
        fake.stop();
        qputenv("HOME", realHome);
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
        // The mode is on OneDrive Settings.
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("oneDriveSettings")));
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
    /// confirmed, forgets this one, and Status offers the new one.
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
        auto *page = window->findChild<QObject *>(QStringLiteral("oneDriveSettingsPage"));
        QObject *another = page ? page->findChild<QObject *>(QStringLiteral("anotherFolderDialog")) : nullptr;
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
        // Asked again and confirmed: forgotten, and Status offers a new folder.
        QMetaObject::invokeMethod(another, "open");
        QTRY_VERIFY(shown(another));
        QMetaObject::invokeMethod(another, "confirm");
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("Unregister")));
        QTRY_VERIFY(!shown(another));
        QTRY_VERIFY(accounts.at(0)->sync()->rootPath().isEmpty());
        fake.sync->folder->decide("offline", "no-folder");
        auto *step = window->findChild<QQuickItem *>(QStringLiteral("folderChoiceCard"));
        QVERIFY(step);
        QTRY_VERIFY(step->isVisible());

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

    /// Issue #57: while the account holds back by itself, the head of Status says why,
    /// with a Sync Anyway button.
    void theStatusShowsAHold()
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
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("status")));
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

    /// What Issues lists that needs the user is one line on Status while there is any, and
    /// it leads there.
    void theStatusLeadsToIssues()
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
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("status")));
        auto *headline = window->findChild<QQuickItem *>(QStringLiteral("headline"));
        auto *issues = window->findChild<QQuickItem *>(QStringLiteral("issuesCard"));
        auto *line = window->findChild<QQuickItem *>(QStringLiteral("issuesLine"));
        QVERIFY(headline && issues && line);
        QTRY_VERIFY(headline->isVisible());
        QVERIFY(!issues->isVisible());

        fake.sync->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(2)}});
        QTRY_VERIFY(issues->isVisible());
        QCOMPARE(line->property("text").toString(), QStringLiteral("Kept both versions of 2 files"));
        QMetaObject::invokeMethod(line, "clicked");
        QTRY_COMPARE(window->property("currentPage").toString(), QStringLiteral("issues"));
        fake.sync->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(0)}});
        QTRY_VERIFY(!issues->isVisible());
        fake.stop();
    }

    /// A folder that stopped (moved, deleted, its disk gone) is fixed from Status: Try Again
    /// in the head, and Use Another Folder… where Open Folder was, which forgets it after the
    /// question, and Status offers a new one. Sync Now and Pause have nothing to do then, and
    /// OneDrive Settings leaves another folder to Status.
    void aStoppedFolderGetsAnotherOnStatus()
    {
        QTemporaryDir home;
        QVERIFY(home.isValid());
        const QByteArray realHome = qgetenv("HOME");
        qputenv("HOME", QFile::encodeName(home.path()));
        const QString root = home.path() + QStringLiteral("/OneDrive");
        FakeDaemon fake;
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        fake.sync->folder->set({{QStringLiteral("Path"), root}, {QStringLiteral("State"), QStringLiteral("ready")}, {QStringLiteral("Source"), QStringLiteral("onedrive")}});
        fake.sync->folder->decide("ok", "up-to-date");
        QVERIFY(fake.start());
        App app;
        QVERIFY(app.load());
        QTRY_COMPARE(app.accounts.count(), 1);
        QTRY_COMPARE(app.accounts.at(0)->sync()->rootPath(), root);
        auto *open = app.find("openFolderButton");
        auto *another = app.findOn("statusPage", "anotherFolderButton");
        auto *syncNow = app.find("syncNowButton");
        auto *pause = app.find("pauseButton");
        auto *tryAgain = app.find("tryAgainButton");
        auto *dialog = app.findOn<QObject>("statusPage", "anotherFolderDialog");
        QVERIFY(open && another && syncNow && pause && tryAgain && dialog);
        QTRY_VERIFY(open->isVisible() && syncNow->isVisible() && pause->isVisible());
        QVERIFY(!another->isVisible() && !tryAgain->isVisible());

        fake.sync->folder->decide("warning", "stopped", {{QStringLiteral("State"), QStringLiteral("error")},
                                                         {QStringLiteral("Trouble"), QString(QStringLiteral("the sync folder is not at ") + root + QStringLiteral(" any more"))}});
        QTRY_VERIFY(another->isVisible() && tryAgain->isVisible());
        QVERIFY(!open->isVisible() && !syncNow->isVisible() && !pause->isVisible());
        QMetaObject::invokeMethod(tryAgain, "clicked");
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("Refresh")));
        QMetaObject::invokeMethod(app.window, "showPage", Q_ARG(QVariant, QStringLiteral("oneDriveSettings")));
        QTRY_COMPARE(app.pageShown(), QStringLiteral("oneDriveSettingsPage"));
        QVERIFY(!app.findOn("oneDriveSettingsPage", "anotherFolderButton")->isVisible());
        QMetaObject::invokeMethod(app.window, "showPage", Q_ARG(QVariant, QStringLiteral("status")));
        QTRY_VERIFY(another->isVisible());

        QMetaObject::invokeMethod(another, "clicked");
        QTRY_VERIFY(shown(dialog));
        QVERIFY(dialog->property("subtitle").toString().startsWith(QStringLiteral("KOneDrive forgets ") + root));
        QMetaObject::invokeMethod(dialog, "confirm");
        QTRY_VERIFY(fake.sync->calls.contains(QStringLiteral("Unregister")));
        QTRY_VERIFY(!shown(dialog));
        fake.sync->folder->decide("offline", "no-folder");
        QTRY_VERIFY(app.find("folderChoiceCard")->isVisible());
        QTRY_COMPARE(app.find("offeredFolder")->property("text").toString(), QStringLiteral("~/OneDrive"));
        fake.stop();
        qputenv("HOME", realHome);
    }

    /// The head of Status says "Read-only for now" while a two-way folder cannot be watched
    /// (Folder.Writable); Now, while the check of the local files runs, since when and why,
    /// and it goes once nothing moves.
    void theStatusShowsReadOnlyAndTheLocalScan()
    {
        FakeDaemon fake;
        fake.account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}, {QStringLiteral("Mode"), QStringLiteral("read-write")}});
        fake.sync->folder->set({{QStringLiteral("Path"), QStringLiteral("/home/u/OneDrive")},
                                {QStringLiteral("State"), QStringLiteral("ready")},
                                {QStringLiteral("Source"), QStringLiteral("onedrive")}});
        fake.sync->scan->set({{QStringLiteral("State"), QStringLiteral("running")},
                              {QStringLiteral("Reason"), QStringLiteral("start")},
                              {QStringLiteral("Started"), QVariant::fromValue<qlonglong>(QDateTime::currentSecsSinceEpoch() - 125)}});
        fake.sync->folder->decide("syncing", "transferring");
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
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("status")));
        auto *detail = window->findChild<QQuickItem *>(QStringLiteral("headDetail"));
        QVERIFY(detail);
        QTRY_VERIFY(accounts.at(0)->account()->mode() == QLatin1String("read-write"));
        QTRY_VERIFY(!detail->property("text").toString().startsWith(QStringLiteral("Read-only")));
        fake.sync->folder->set({{QStringLiteral("Writable"), false}});
        QTRY_COMPARE(detail->property("text").toString(), QStringLiteral("Read-only for now"));
        fake.sync->folder->set({{QStringLiteral("Writable"), true}});
        QTRY_VERIFY(!detail->property("text").toString().startsWith(QStringLiteral("Read-only")));
        auto *now = window->findChild<QQuickItem *>(QStringLiteral("nowCard"));
        auto *line = window->findChild<QQuickItem *>(QStringLiteral("scanLine"));
        QVERIFY(now && line);
        QTRY_VERIFY(now->isVisible() && line->isVisible());
        QCOMPARE(line->property("text").toString(),
                 QStringLiteral("Checking local files — started 2 min ago, as syncing started"));

        // A check that ended says nothing: what changes after it is seen as it is made.
        fake.sync->scan->set({{QStringLiteral("State"), QStringLiteral("idle")},
                              {QStringLiteral("Finished"), QVariant::fromValue<qlonglong>(QDateTime::currentSecsSinceEpoch() - 300)},
                              {QStringLiteral("Took"), QVariant::fromValue<uint>(40)}});
        QTRY_VERIFY(!line->isVisible());
        QVERIFY(!now->isVisible());
        fake.stop();
    }

    /// With no service, Status says so and starts it; a start refused (the test bus
    /// has no systemd) says it could not, and offers to start again.
    void theStatusStartsTheService()
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
    /// refuses, Issues and Status come up at once and build a handful of rows
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
        auto *perFile = window->findChild<QQuickItem *>(QStringLiteral("perFileGroup"));
        QVERIFY(perFile);
        QTRY_COMPARE(itemsSaying(perFile, root), 3);
        QVERIFY(fake.sync->calls.contains(QStringLiteral("NotUploadedFiles:name-characters:20")));
        QVERIFY(!fake.sync->calls.join(QLatin1Char(' ')).contains(QStringLiteral("NotUploadedFiles:quota-exceeded")));

        // Status: what is left is in Now (issue #16), and one line leads to what can't upload.
        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("status")));
        auto *status = window->findChild<QQuickItem *>(QStringLiteral("statusPage"));
        QVERIFY(status);
        auto *link = window->findChild<QQuickItem *>(QStringLiteral("issuesLine"));
        QVERIFY(link);
        QTRY_VERIFY(link->isVisible());
        QVERIFY2(link->property("text").toString().contains(QStringLiteral("005 changes can't upload")), qPrintable(link->property("text").toString()));
        const auto left = [window](const char *name) {
            auto *item = window->findChild<QQuickItem *>(QLatin1String(name));
            return item && item->isVisible() ? item->property("description").toString() : QString();
        };
        // Sizes as the locale writes them, and 1234 with or without its thousands apart.
        const QLocale locale;
        const QString down = left("downloadLine");
        QVERIFY2(down.startsWith(QLatin1Char('1'))
                     && down.endsWith(QStringLiteral("234 files left · ") + locale.formattedDataSize(3ULL << 30) + QStringLiteral(" · about 12 min")),
                 qPrintable(down));
        QCOMPARE(left("uploadLine"), QStringLiteral("2 changes left · ") + locale.formattedDataSize(1024));
        QCOMPARE(itemsSaying(status, root + QLatin1Char('/')), 0);
        QVERIFY(itemsSaying(status, QString()) <= 20);

        // Nothing left to download: its line goes.
        fake.sync->transfers->set({{QStringLiteral("DownloadLeftCount"), QVariant::fromValue<uint>(0)}});
        QTRY_VERIFY(left("downloadLine").isEmpty());
        QVERIFY(!left("uploadLine").isEmpty());

        QVERIFY(!fake.sync->calls.contains(QStringLiteral("Changes")));
        QVERIFY(!fake.sync->calls.contains(QStringLiteral("NotUploaded")));
        fake.stop();
    }

    /// Issue #39: 5 000 skipped entries and 5 000 conflicts are the first
    /// 200 of each in Issues' one list, which builds only the rows in sight,
    /// each followed by "and 4800 more"; a count that keeps moving asks for
    /// the skipped list again at most once a second.
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

        QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, QStringLiteral("issues")));
        auto *list = window->findChild<QQuickItem *>(QStringLiteral("issuesList"));
        QVERIFY(list);
        QTRY_COMPARE(sync->skipped().size(), 5000);
        QTRY_COMPARE(sync->conflicts()->count(), 200);
        // Each section: its heading, 200 rows and the line that counts the rest.
        QTRY_COMPARE(list->property("count").toInt(), 2 * (1 + 200 + 1));
        // Taller rows first: a few in sight, and a few more built beyond them.
        QTRY_VERIFY(builtRows(list, QStringLiteral("conflictRow")) >= 3);
        QTest::qWait(100);
        QVERIFY2(builtRows(list, QStringLiteral("conflictRow")) <= 20, "more rows built than a few screens hold");
        // After the conflicts, the line that counts the rest of them.
        QMetaObject::invokeMethod(list, "positionViewAtIndex", Q_ARG(int, 201), Q_ARG(int, 1));
        const auto shownRow = [list](const char *name) -> QQuickItem * {
            const auto *content = list->property("contentItem").value<QQuickItem *>();
            const auto children = content ? content->childItems() : QList<QQuickItem *>();
            for (QQuickItem *item : children) {
                if (item->objectName() == QLatin1String(name) && item->isVisible()) {
                    return item;
                }
            }
            return nullptr;
        };
        QTRY_VERIFY(shownRow("conflictsMore"));
        auto *conflictsMore = shownRow("conflictsMore");
        QTest::qWait(100);
        QVERIFY2(builtRows(list, QStringLiteral("conflictRow")) + builtRows(list, QStringLiteral("skippedRow")) <= 40, "more rows built than a few screens hold");
        QCOMPARE(digits(conflictsMore->property("text").toString()), QStringLiteral("4800"));
        QVERIFY(conflictsMore->property("description").toString().contains(QStringLiteral("konedrivectl sync conflicts")));
        // At the end, the line that counts the rest of the skipped.
        QMetaObject::invokeMethod(list, "positionViewAtEnd");
        QTRY_VERIFY(shownRow("skippedMore"));
        auto *skippedMore = shownRow("skippedMore");
        QTest::qWait(100);
        QVERIFY2(builtRows(list, QStringLiteral("conflictRow")) + builtRows(list, QStringLiteral("skippedRow")) <= 40, "more rows built than a few screens hold");
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
        fake.stop();
    }
};

QTEST_MAIN(DialogsTest)

#include "dialogstest.moc"
