// The context menu plugin as KFileItemActions loads and uses it
// (kio src/widgets/kfileitemactions.cpp), against a stand-in for konedrived
// on a private session bus.

#include "syncclient.h"
#include "testsupport.h"

#include <KAbstractFileItemActionPlugin>
#include <KFileItem>
#include <KFileItemActions>
#include <KFileItemListProperties>
#include <KPluginFactory>
#include <KPluginMetaData>

#include <QAction>
#include <QDBusConnection>
#include <QDBusConnectionInterface>
#include <QElapsedTimer>
#include <QMenu>
#include <QScopeGuard>
#include <QSignalSpy>
#include <QStandardPaths>
#include <QTest>

#include <memory>

#include <sys/stat.h>

using namespace testsupport;

namespace
{
const QString Download = QStringLiteral("konedrive_download");
const QString FreeUpSpace = QStringLiteral("konedrive_free_up_space");
const QString DaemonService = QStringLiteral("org.konedrive.Daemon");
} // namespace

class ActionPluginTest : public QObject
{
    Q_OBJECT

    std::unique_ptr<FakeSync> m_fake;

    bool startFake()
    {
        m_fake = std::make_unique<FakeSync>();
        return m_fake->start();
    }

    /// As KFileItemActions does it.
    KAbstractFileItemActionPlugin *createPlugin()
    {
        const KPluginMetaData metaData(QStringLiteral(KONEDRIVE_ACTIONS_PLUGIN));
        return KPluginFactory::instantiatePlugin<KAbstractFileItemActionPlugin>(metaData, this).plugin;
    }

    static KFileItemListProperties selection(const QStringList &paths)
    {
        KFileItemList items;
        for (const QString &path : paths) {
            items.append(KFileItem(url(path), QStringLiteral("application/octet-stream")));
        }
        return KFileItemListProperties(items);
    }

    static QStringList names(const QList<QAction *> &actions)
    {
        QStringList result;
        for (const QAction *action : actions) {
            result.append(action->objectName());
        }
        return result;
    }

    static QAction *find(const QList<QAction *> &actions, const QString &name)
    {
        for (QAction *action : actions) {
            if (action->objectName() == name) {
                return action;
            }
        }
        return nullptr;
    }

private Q_SLOTS:
    void initTestCase()
    {
        QStandardPaths::setTestModeEnabled(true);
        QVERIFY(createPlugin());
    }

    void cleanup()
    {
        m_fake.reset();
    }

    void offeredActionFollowsTheState_data()
    {
        QTest::addColumn<QByteArray>("state");
        QTest::addColumn<bool>("inRoot");
        QTest::addColumn<QStringList>("expected");
        QTest::newRow("online-only: Download") << QByteArray("online-only") << true << QStringList{Download};
        QTest::newRow("hydrated: Free up space") << QByteArray("hydrated") << true << QStringList{FreeUpSpace};
        QTest::newRow("hydrating: nothing") << QByteArray("hydrating") << true << QStringList();
        QTest::newRow("dehydrating: nothing") << QByteArray("dehydrating") << true << QStringList();
        QTest::newRow("no attribute: nothing") << QByteArray() << true << QStringList();
        QTest::newRow("a value we do not know: nothing") << QByteArray("downloaded") << true << QStringList();
        QTest::newRow("online-only outside a root: nothing") << QByteArray("online-only") << false << QStringList();
        QTest::newRow("hydrated outside a root: nothing") << QByteArray("hydrated") << false << QStringList();
    }

    void offeredActionFollowsTheState()
    {
        QFETCH(QByteArray, state);
        QFETCH(bool, inRoot);
        QFETCH(QStringList, expected);
        Tree tree;
        QVERIFY(inRoot ? tree.root(QStringLiteral("top")) : tree.dir(QStringLiteral("top")));
        QVERIFY(tree.file(QStringLiteral("top/doc.bin"), state));

        const QList<QAction *> actions = createPlugin()->actions(selection({tree.path(QStringLiteral("top/doc.bin"))}), nullptr);
        QCOMPARE(names(actions), expected);
        if (QAction *download = find(actions, Download)) {
            QCOMPARE(download->text(), QStringLiteral("Download"));
        }
        if (QAction *freeUp = find(actions, FreeUpSpace)) {
            QCOMPARE(freeUp->text(), QStringLiteral("Free up space"));
        }
    }

    /// Neither a symbolic link to a placeholder nor a folder is offered
    /// anything.
    void nothingForLinkOrFolder()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        QVERIFY(tree.symlink(QStringLiteral("OneDrive/doc.bin"), QStringLiteral("OneDrive/link.bin")));
        QVERIFY(tree.dir(QStringLiteral("OneDrive/folder")));
        QVERIFY(setState(tree.path(QStringLiteral("OneDrive/folder")), "online-only"));

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QCOMPARE(names(plugin->actions(selection({tree.path(QStringLiteral("OneDrive/link.bin"))}), nullptr)), QStringList());
        const KFileItem folder(url(tree.path(QStringLiteral("OneDrive/folder"))), QStringLiteral("inode/directory"), S_IFDIR);
        QCOMPARE(names(plugin->actions(KFileItemListProperties({folder}), nullptr)), QStringList());
    }

    void nothingThroughALinkOutOfTheRoot_data()
    {
        QTest::addColumn<QString>("shownAs");
        QTest::newRow("through a link in the root to a folder outside") << QStringLiteral("OneDrive/escape/f.bin");
        QTest::newRow("through .. out of the root") << QStringLiteral("OneDrive/../Elsewhere/f.bin");
    }

    /// A placeholder outside the sync folder, reached through a symbolic
    /// link inside it, is not offered Download: it is not in the folder.
    void nothingThroughALinkOutOfTheRoot()
    {
        QFETCH(QString, shownAs);
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("Elsewhere/f.bin"), "online-only"));
        QVERIFY(tree.symlink(QStringLiteral("Elsewhere"), QStringLiteral("OneDrive/escape")));
        QCOMPARE(names(createPlugin()->actions(selection({tree.path(shownAs)}), nullptr)), QStringList());
    }

    /// With several files selected each action applies to exactly the files
    /// it fits, and one that fits none is not shown.
    void severalFilesEachActionForTheFilesItFits()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/a.bin"), "online-only"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/b.bin"), "online-only"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/c.bin"), "hydrated"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/d.bin"), "hydrating"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/e.txt")));
        QVERIFY(tree.file(QStringLiteral("Elsewhere/f.bin"), "online-only"));
        const auto p = [&tree](const char *name) {
            return tree.path(QString::fromLatin1(name));
        };
        QVERIFY(startFake());

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        const QStringList all{p("OneDrive/a.bin"), p("OneDrive/b.bin"), p("OneDrive/c.bin"), p("OneDrive/d.bin"), p("OneDrive/e.txt"), p("Elsewhere/f.bin")};
        const QList<QAction *> both = plugin->actions(selection(all), nullptr);
        QCOMPARE(names(both), (QStringList{Download, FreeUpSpace}));

        find(both, Download)->trigger();
        QTRY_COMPARE(m_fake->calls, (QStringList{QStringLiteral("Hydrate ") + p("OneDrive/a.bin"), QStringLiteral("Hydrate ") + p("OneDrive/b.bin")}));
        find(both, FreeUpSpace)->trigger();
        QTRY_COMPARE(m_fake->calls.size(), 3);
        QCOMPARE(m_fake->calls.at(2), QStringLiteral("Dehydrate ") + p("OneDrive/c.bin"));

        const QList<QAction *> downloadOnly = plugin->actions(selection({p("OneDrive/a.bin"), p("OneDrive/d.bin"), p("OneDrive/e.txt")}), nullptr);
        QCOMPARE(names(downloadOnly), QStringList{Download});
        const QList<QAction *> freeUpOnly = plugin->actions(selection({p("OneDrive/c.bin"), p("OneDrive/d.bin"), p("Elsewhere/f.bin")}), nullptr);
        QCOMPARE(names(freeUpOnly), QStringList{FreeUpSpace});
        QCOMPARE(names(plugin->actions(selection({p("OneDrive/d.bin"), p("OneDrive/e.txt"), p("Elsewhere/f.bin")}), nullptr)), QStringList());
    }

    /// The menu KFileItemActions builds -- the one Dolphin shows -- has the
    /// actions for files of any type, and for the files of a selection that
    /// also holds a folder.
    void inTheContextMenuKioBuilds_data()
    {
        QTest::addColumn<QStringList>("entries"); // name:mimetype:state, or name/ for a folder
        QTest::addColumn<QStringList>("expected");
        QTest::newRow("one text file") << QStringList{QStringLiteral("notes.txt:text/plain:online-only")} << QStringList{Download};
        QTest::newRow("files of different types")
            << QStringList{QStringLiteral("notes.txt:text/plain:online-only"),
                           QStringLiteral("photo.jpg:image/jpeg:online-only"),
                           QStringLiteral("report.pdf:application/pdf:hydrated")}
            << QStringList{Download, FreeUpSpace};
        QTest::newRow("a file and a folder") << QStringList{QStringLiteral("notes.txt:text/plain:online-only"), QStringLiteral("sub/")} << QStringList{Download};
    }

    void inTheContextMenuKioBuilds()
    {
        QFETCH(QStringList, entries);
        QFETCH(QStringList, expected);
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        KFileItemList items;
        for (const QString &entry : std::as_const(entries)) {
            if (entry.endsWith(QLatin1Char('/'))) {
                QVERIFY(tree.dir(QStringLiteral("OneDrive/") + entry));
                items.append(KFileItem(url(tree.path(QStringLiteral("OneDrive/") + entry.chopped(1))), QStringLiteral("inode/directory"), S_IFDIR));
                continue;
            }
            const QStringList parts = entry.split(QLatin1Char(':'));
            QVERIFY(tree.file(QStringLiteral("OneDrive/") + parts.at(0), parts.at(2).toLatin1()));
            items.append(KFileItem(url(tree.path(QStringLiteral("OneDrive/") + parts.at(0))), parts.at(1), S_IFREG));
        }

        // Only the plugins built here, not whatever else is installed.
        const QStringList saved = QCoreApplication::libraryPaths();
        QCoreApplication::setLibraryPaths({QStringLiteral(KONEDRIVE_PLUGIN_DIR)});
        const auto restore = qScopeGuard([&saved]() {
            QCoreApplication::setLibraryPaths(saved);
        });

        KFileItemActions fileItemActions;
        fileItemActions.setItemListProperties(KFileItemListProperties(items));
        QMenu menu;
        fileItemActions.addActionsTo(&menu, KFileItemActions::MenuActionSource::Plugins);
        QStringList shown;
        const QList<QAction *> actions = menu.findChildren<QAction *>() + menu.actions();
        for (const QAction *action : actions) {
            if (action->objectName().startsWith(QLatin1String("konedrive_")) && !shown.contains(action->objectName())) {
                shown.append(action->objectName());
            }
        }
        QCOMPARE(shown, expected);
    }

    /// Choosing an action returns at once; the daemon's answer, however long
    /// it takes, arrives later on the event loop.
    void callsDoNotBlock()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));
        QVERIFY(startFake());
        m_fake->answers.insert(doc, {QString(), QString(), 2000});

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        QAction *download = find(plugin->actions(selection({doc}), nullptr), Download);
        QVERIFY(download);

        QElapsedTimer clock;
        clock.start();
        download->trigger();
        const qint64 took = clock.elapsed();
        QVERIFY2(took < 500, qPrintable(QStringLiteral("trigger() took %1 ms").arg(took)));

        QTRY_COMPARE(m_fake->calls, QStringList{QStringLiteral("Hydrate ") + doc});
        QCOMPARE(m_fake->delayedAnswersSent, 0);
        QTRY_COMPARE_WITH_TIMEOUT(m_fake->delayedAnswersSent, 1, 5000);
        QTest::qWait(konedrive::SyncClient::ReportDelayMs + 200);
        QCOMPARE(errors.count(), 0);
    }

    void refusalIsExplained_data()
    {
        QTest::addColumn<bool>("download");
        QTest::addColumn<QString>("errorName");
        QTest::addColumn<QString>("message");
        QTest::addColumn<QString>("expected");
        QTest::addColumn<bool>("daemonWordsShown");

        const QString decoy = QStringLiteral("DAEMON-OWN-WORDS");
        const auto named = [](const char *name) {
            return QStringLiteral("org.konedrive.Error.") + QLatin1String(name);
        };
        QTest::newRow("Download: NoHelper") << true << named("NoHelper") << decoy << QStringLiteral("must first have the helper stop letting it through unchecked") << false;
        QTest::newRow("Download: NoRoot") << true << named("NoRoot") << decoy << QStringLiteral("KOneDrive has no sync folder registered") << false;
        QTest::newRow("Download: NoSource") << true << named("NoSource") << decoy << QStringLiteral("does not know where to download “doc.bin” from yet") << false;
        QTest::newRow("Download: OutsideRoot") << true << named("OutsideRoot") << decoy << QStringLiteral("is not a regular file inside KOneDrive's sync folder") << false;
        QTest::newRow("Download: NotManaged") << true << named("NotManaged") << decoy << QStringLiteral("so there is nothing to download") << false;
        QTest::newRow("Download: ModifiedLocally") << true << named("ModifiedLocally") << decoy << QStringLiteral("downloading it again would overwrite your edits") << false;
        QTest::newRow("Download: Failed") << true << named("Failed") << QStringLiteral("disk full") << QStringLiteral("Downloading “doc.bin” failed: disk full") << true;
        QTest::newRow("Download: a name that is not ours")
            << true << QStringLiteral("org.freedesktop.DBus.Error.InUse") << QStringLiteral("something else entirely")
            << QStringLiteral("Downloading “doc.bin” failed: something else entirely") << true;
        QTest::newRow("Free up: NoHelper") << false << named("NoHelper") << decoy << QStringLiteral("must first have the helper take off any mark") << false;
        QTest::newRow("Free up: NoRoot") << false << named("NoRoot") << decoy << QStringLiteral("KOneDrive has no sync folder registered") << false;
        QTest::newRow("Free up: OutsideRoot") << false << named("OutsideRoot") << decoy << QStringLiteral("is not a regular file inside KOneDrive's sync folder") << false;
        QTest::newRow("Free up: NotManaged") << false << named("NotManaged") << decoy << QStringLiteral("never frees the space of a file it could not download again") << false;
        QTest::newRow("Free up: NotHydrated") << false << named("NotHydrated") << decoy << QStringLiteral("is not downloaded, so there is no space to free") << false;
        QTest::newRow("Free up: ModifiedLocally") << false << named("ModifiedLocally") << decoy << QStringLiteral("freeing its space would lose your edits") << false;
        QTest::newRow("Free up: InUse") << false << named("InUse") << decoy << QStringLiteral("is open in another program, so its space cannot be freed") << false;
        QTest::newRow("Free up: Failed") << false << named("Failed") << QStringLiteral("disk full") << QStringLiteral("Freeing up “doc.bin” failed: disk full") << true;
    }

    /// Each refusal is explained by its name, in words about the user's file.
    void refusalIsExplained()
    {
        QFETCH(bool, download);
        QFETCH(QString, errorName);
        QFETCH(QString, message);
        QFETCH(QString, expected);
        QFETCH(bool, daemonWordsShown);
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), download ? "online-only" : "hydrated"));
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));
        QVERIFY(startFake());
        m_fake->answers.insert(doc, {errorName, message, 0});

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        QAction *action = find(plugin->actions(selection({doc}), nullptr), download ? Download : FreeUpSpace);
        QVERIFY(action);
        action->trigger();
        QTRY_COMPARE(errors.count(), 1);
        const QString text = errors.at(0).at(0).toString();
        QVERIFY2(text.contains(expected), qPrintable(text));
        QVERIFY2(text.contains(QStringLiteral("“doc.bin”")), qPrintable(text));
        QCOMPARE(text.contains(message), daemonWordsShown);
    }

    void daemonNotRunning_data()
    {
        QTest::addColumn<bool>("download");
        QTest::newRow("Download") << true;
        QTest::newRow("Free up space") << false;
    }

    /// With no daemon on the bus the action says so, plainly.
    void daemonNotRunning()
    {
        QFETCH(bool, download);
        QVERIFY(!QDBusConnection::sessionBus().interface()->isServiceRegistered(DaemonService));
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), download ? "online-only" : "hydrated"));

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        QAction *action = find(plugin->actions(selection({tree.path(QStringLiteral("OneDrive/doc.bin"))}), nullptr), download ? Download : FreeUpSpace);
        QVERIFY(action);
        action->trigger();
        QTRY_COMPARE(errors.count(), 1);
        const QString text = errors.at(0).at(0).toString();
        QVERIFY2(text.startsWith(QStringLiteral("KOneDrive is not running, so ")), qPrintable(text));
        QVERIFY2(text.contains(QStringLiteral("“doc.bin”")), qPrintable(text));
        QVERIFY2(text.contains(QStringLiteral("systemctl --user start konedrived")), qPrintable(text));
    }

    /// The daemon can be started on demand, but starting it fails: that is
    /// the daemon not running too, not an unexplained bus error.
    void daemonFailsToStart()
    {
        if (qEnvironmentVariableIsEmpty("KONEDRIVE_TEST_ACTIVATING_BUS")) {
            QSKIP("needs a bus where the daemon is activatable; ctest runs it as actionplugintest_activation");
        }
        QVERIFY(QDBusConnection::sessionBus().interface()->activatableServiceNames().value().contains(DaemonService));
        const QDBusMessage probe = QDBusConnection::sessionBus().call(
            QDBusMessage::createMethodCall(DaemonService, QStringLiteral("/org/konedrive/Daemon"), QStringLiteral("org.konedrive.Sync1"), QStringLiteral("Hydrate"))
            << QStringLiteral("/nonexistent"));
        qInfo("the bus answers a failed start with %s", qPrintable(probe.errorName()));

        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection({tree.path(QStringLiteral("OneDrive/doc.bin"))}), nullptr), Download)->trigger();
        QTRY_COMPARE(errors.count(), 1);
        const QString text = errors.at(0).at(0).toString();
        QVERIFY2(text.startsWith(QStringLiteral("KOneDrive is not running, so “doc.bin” was not downloaded")), qPrintable(text));
    }

    /// The daemon leaves the bus while a download waits for it.
    void daemonStopsBeforeAnswering()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));
        QVERIFY(startFake());
        m_fake->answers.insert(doc, {QString(), QString(), 60000});

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection({doc}), nullptr), Download)->trigger();
        QTRY_COMPARE(m_fake->calls.size(), 1);
        m_fake->stop();
        QTRY_COMPARE(errors.count(), 1);
        const QString text = errors.at(0).at(0).toString();
        QVERIFY2(text.startsWith(QStringLiteral("KOneDrive stopped before it finished downloading “doc.bin”")), qPrintable(text));
    }

    /// Three files refused for one reason make one message, not three.
    void oneMessageForSeveralFiles()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QStringList paths;
        for (const char *name : {"a.bin", "b.bin", "c.bin"}) {
            QVERIFY(tree.file(QStringLiteral("OneDrive/") + QLatin1String(name), "online-only"));
            paths.append(tree.path(QStringLiteral("OneDrive/") + QLatin1String(name)));
        }
        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection(paths), nullptr), Download)->trigger();
        QTRY_COMPARE(errors.count(), 1);
        QTest::qWait(3 * konedrive::SyncClient::ReportDelayMs);
        QCOMPARE(errors.count(), 1);
        const QString text = errors.at(0).at(0).toString();
        QVERIFY2(text.startsWith(QStringLiteral("KOneDrive is not running, so “a.bin” was not downloaded")), qPrintable(text));
        QVERIFY2(text.contains(QStringLiteral("2 more files were not downloaded for the same reason.")), qPrintable(text));
    }

    /// Several files refused for different reasons: each reason once, for
    /// the first file it happened to, with a count of the rest.
    void eachReasonExplainedOnce()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QStringList paths;
        for (const char *name : {"a.bin", "b.bin", "c.bin"}) {
            QVERIFY(tree.file(QStringLiteral("OneDrive/") + QLatin1String(name), "hydrated"));
            paths.append(tree.path(QStringLiteral("OneDrive/") + QLatin1String(name)));
        }
        QVERIFY(startFake());
        m_fake->answers.insert(paths.at(0), {QStringLiteral("org.konedrive.Error.ModifiedLocally"), QStringLiteral("modified"), 0});
        m_fake->answers.insert(paths.at(1), {QStringLiteral("org.konedrive.Error.InUse"), QStringLiteral("in use"), 0});
        m_fake->answers.insert(paths.at(2), {QStringLiteral("org.konedrive.Error.ModifiedLocally"), QStringLiteral("modified"), 0});

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection(paths), nullptr), FreeUpSpace)->trigger();
        QTRY_COMPARE(errors.count(), 1);
        const QString text = errors.at(0).at(0).toString();
        QVERIFY2(text.contains(QStringLiteral("“a.bin” was changed here and has not been uploaded, so freeing its space would lose your edits.")), qPrintable(text));
        QVERIFY2(text.contains(QStringLiteral("One more file was not freed up for the same reason.")), qPrintable(text));
        QVERIFY2(text.contains(QStringLiteral("“b.bin” is open in another program")), qPrintable(text));
        QVERIFY2(!text.contains(QStringLiteral("“c.bin”")), qPrintable(text));
    }

    /// The daemon's own message rarely ends a sentence; the count that
    /// follows it still starts one.
    void countAfterTheDaemonsWordsStartsASentence()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/a.bin"), "online-only"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/b.bin"), "online-only"));
        const QStringList paths{tree.path(QStringLiteral("OneDrive/a.bin")), tree.path(QStringLiteral("OneDrive/b.bin"))};
        QVERIFY(startFake());
        m_fake->defaultAnswer = {QStringLiteral("org.konedrive.Error.Failed"), QStringLiteral("disk full"), 0};

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection(paths), nullptr), Download)->trigger();
        QTRY_COMPARE(errors.count(), 1);
        QCOMPARE(errors.at(0).at(0).toString(), QStringLiteral("Downloading “a.bin” failed: disk full. One more file was not downloaded for the same reason."));
    }

    /// A file refused at once is reported at once, not after another file
    /// of the same request finishes downloading.
    void refusalIsNotHeldBackBySlowDownload()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/a.bin"), "online-only"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/b.bin"), "online-only"));
        const QString a = tree.path(QStringLiteral("OneDrive/a.bin"));
        const QString b = tree.path(QStringLiteral("OneDrive/b.bin"));
        QVERIFY(startFake());
        m_fake->answers.insert(a, {QStringLiteral("org.konedrive.Error.ModifiedLocally"), QStringLiteral("modified"), 0});
        m_fake->answers.insert(b, {QString(), QString(), 5000});

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        QElapsedTimer clock;
        clock.start();
        find(plugin->actions(selection({a, b}), nullptr), Download)->trigger();
        QTRY_COMPARE_WITH_TIMEOUT(errors.count(), 1, 2000);
        QVERIFY2(clock.elapsed() < 2000, qPrintable(QStringLiteral("reported after %1 ms").arg(clock.elapsed())));
        QVERIFY(errors.at(0).at(0).toString().contains(QStringLiteral("“a.bin” was changed here")));

        QTRY_COMPARE_WITH_TIMEOUT(m_fake->delayedAnswersSent, 1, 7000);
        QTest::qWait(konedrive::SyncClient::ReportDelayMs + 200);
        QCOMPARE(errors.count(), 1);
    }

    /// A file whose request the daemon has not answered yet is not asked for
    /// again -- repeated clicks on a daemon that hangs would otherwise pile
    /// up calls in Dolphin -- and the user is told why nothing happened.
    void fileAlreadyWaitingIsNotAskedAgain()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/new.bin"), "online-only"));
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));
        const QString fresh = tree.path(QStringLiteral("OneDrive/new.bin"));
        QVERIFY(startFake());
        m_fake->defaultAnswer = {QString(), QString(), -1};

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection({doc}), nullptr), Download)->trigger();
        QTRY_COMPARE(m_fake->calls, QStringList{QStringLiteral("Hydrate ") + doc});

        find(plugin->actions(selection({doc}), nullptr), Download)->trigger();
        QTest::qWait(konedrive::SyncClient::ReportDelayMs + 200);
        QCOMPARE(m_fake->calls.size(), 1);
        QTRY_COMPARE(errors.count(), 1);
        QVERIFY2(errors.at(0).at(0).toString().startsWith(QStringLiteral("KOneDrive has not yet answered an earlier request for “doc.bin”, so it was not asked again.")),
                 qPrintable(errors.at(0).at(0).toString()));

        // With a file not asked for yet: that one is sent, the other is not.
        find(plugin->actions(selection({doc, fresh}), nullptr), Download)->trigger();
        QTRY_COMPARE(errors.count(), 2);
        QVERIFY2(!errors.at(1).at(0).toString().contains(QStringLiteral("“new.bin”")), qPrintable(errors.at(1).at(0).toString()));
        QTest::qWait(konedrive::SyncClient::ReportDelayMs + 200);
        QCOMPARE(m_fake->calls, (QStringList{QStringLiteral("Hydrate ") + doc, QStringLiteral("Hydrate ") + fresh}));
    }

    /// However many files are chosen, no more than MaxCallsInFlight calls
    /// wait for the daemon at once: they cost Dolphin memory, and on a
    /// dbus-daemon bus they share Dolphin's own budget of pending replies.
    /// The rest are refused with a message that says so.
    void callsInFlightAreCapped()
    {
        const int cap = konedrive::SyncClient::MaxCallsInFlight;
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QStringList paths;
        for (int i = 0; i <= cap; ++i) {
            const QString name = QStringLiteral("OneDrive/f%1.bin").arg(i, 5, 10, QLatin1Char('0'));
            QVERIFY(tree.file(name, "online-only"));
            paths.append(tree.path(name));
        }
        QVERIFY(startFake());
        m_fake->defaultAnswer = {QString(), QString(), -1};

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection(paths), nullptr), Download)->trigger();
        QTRY_VERIFY_WITH_TIMEOUT(m_fake->calls.size() >= cap, 10000);
        QTest::qWait(konedrive::SyncClient::ReportDelayMs + 200);
        QCOMPARE(m_fake->calls.size(), cap);
        QVERIFY(!m_fake->calls.contains(QStringLiteral("Hydrate ") + paths.last()));
        QTRY_COMPARE(errors.count(), 1);
        const QString text = errors.at(0).at(0).toString();
        // The count is written the locale's way ("1,000" here).
        QVERIFY2(QString(text).remove(QLatin1Char(',')).startsWith(QString::number(cap) + QLatin1Char(' ')), qPrintable(text));
        QVERIFY2(text.contains(QStringLiteral(" requests to KOneDrive are already waiting for an answer, so “f%1.bin” was not downloaded.")
                                   .arg(cap, 5, 10, QLatin1Char('0'))),
                 qPrintable(text));
    }

    /// A download that takes longer than D-Bus's default 25-second reply
    /// timeout is not reported as a failure.
    void downloadLongerThanTheDefaultTimeoutIsNotAFailure()
    {
        if (qEnvironmentVariableIsEmpty("KONEDRIVE_TEST_SLOW")) {
            QSKIP("takes 30 s; ctest runs it as actionplugintest_slow");
        }
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));
        QVERIFY(startFake());
        m_fake->answers.insert(doc, {QString(), QString(), 30000});

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection({doc}), nullptr), Download)->trigger();
        QTRY_COMPARE_WITH_TIMEOUT(m_fake->delayedAnswersSent, 1, 40000);
        QTest::qWait(konedrive::SyncClient::ReportDelayMs + 500);
        QVERIFY2(errors.isEmpty(), errors.isEmpty() ? "" : qPrintable(errors.at(0).at(0).toString()));
    }
};

QTEST_MAIN(ActionPluginTest)

#include "actionplugintest.moc"
