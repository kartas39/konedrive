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
const QString AlwaysKeep = QStringLiteral("konedrive_always_keep");
const QString FreeUp = QStringLiteral("konedrive_free_up_space");
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

    /// Single-item selections: what each of the four pin states offers.
    void menuFollowsTheState_data()
    {
        QTest::addColumn<QByteArray>("state");
        QTest::addColumn<QString>("pin"); // "none", "explicit", "ancestor"
        QTest::addColumn<bool>("inRoot");
        QTest::addColumn<bool>("expectAlwaysKeep");
        QTest::addColumn<bool>("expectChecked");
        QTest::addColumn<bool>("expectAlwaysKeepEnabled");
        QTest::addColumn<bool>("expectFreeUp");
        QTest::addColumn<bool>("expectFreeUpEnabled");

        QTest::newRow("online-only, not pinned") << QByteArray("online-only") << QStringLiteral("none") << true << true << false << true << false << true;
        QTest::newRow("online-only, explicitly pinned") << QByteArray("online-only") << QStringLiteral("explicit") << true << true << true << true << true << true;
        QTest::newRow("hydrated, not pinned") << QByteArray("hydrated") << QStringLiteral("none") << true << true << false << true << true << true;
        QTest::newRow("hydrated, explicitly pinned") << QByteArray("hydrated") << QStringLiteral("explicit") << true << true << true << true << true << true;
        QTest::newRow("hydrated, pinned by an ancestor") << QByteArray("hydrated") << QStringLiteral("ancestor") << true << true << true << false << true << false;
        QTest::newRow("hydrated, not in a root") << QByteArray("hydrated") << QStringLiteral("none") << false << false << false << true << false << true;
    }

    void menuFollowsTheState()
    {
        QFETCH(QByteArray, state);
        QFETCH(QString, pin);
        QFETCH(bool, inRoot);
        QFETCH(bool, expectAlwaysKeep);
        QFETCH(bool, expectChecked);
        QFETCH(bool, expectAlwaysKeepEnabled);
        QFETCH(bool, expectFreeUp);
        QFETCH(bool, expectFreeUpEnabled);

        Tree tree;
        QVERIFY(inRoot ? tree.root(QStringLiteral("OneDrive")) : tree.dir(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), state));
        if (pin == QLatin1String("explicit")) {
            QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive/doc.bin"))));
        } else if (pin == QLatin1String("ancestor")) {
            QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive"))));
        }

        const QList<QAction *> actions = createPlugin()->actions(selection({tree.path(QStringLiteral("OneDrive/doc.bin"))}), nullptr);
        QAction *alwaysKeep = find(actions, AlwaysKeep);
        QCOMPARE(alwaysKeep != nullptr, expectAlwaysKeep);
        if (alwaysKeep) {
            QVERIFY(alwaysKeep->isCheckable());
            QCOMPARE(alwaysKeep->isChecked(), expectChecked);
            QCOMPARE(alwaysKeep->isEnabled(), expectAlwaysKeepEnabled);
            if (!expectAlwaysKeepEnabled) {
                QVERIFY2(alwaysKeep->toolTip().contains(QStringLiteral("OneDrive")), qPrintable(alwaysKeep->toolTip()));
            }
        }
        QAction *freeUp = find(actions, FreeUp);
        QCOMPARE(freeUp != nullptr, expectFreeUp);
        if (freeUp) {
            QCOMPARE(freeUp->isEnabled(), expectFreeUpEnabled);
        }
    }

    /// A selection with one pinned and one unpinned file: not fully checked
    /// (it is not "the selection" that is pinned), and not disabled (some of
    /// it can still usefully be toggled).
    void mixedSelectionIsNeitherFullyCheckedNorDisabled()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/pinned.bin"), "hydrated"));
        QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive/pinned.bin"))));
        QVERIFY(tree.file(QStringLiteral("OneDrive/plain.bin"), "hydrated"));

        const QList<QAction *> actions =
            createPlugin()->actions(selection({tree.path(QStringLiteral("OneDrive/pinned.bin")), tree.path(QStringLiteral("OneDrive/plain.bin"))}), nullptr);
        QAction *alwaysKeep = find(actions, AlwaysKeep);
        QVERIFY(alwaysKeep);
        QVERIFY(!alwaysKeep->isChecked());
        QVERIFY(alwaysKeep->isEnabled());
        QAction *freeUp = find(actions, FreeUp);
        QVERIFY(freeUp);
        QVERIFY(freeUp->isEnabled());
    }

    /// Every item pinned, and only by the same ancestor folder: "Always
    /// keep" is checked (the selection is effectively pinned) but disabled
    /// (nothing would change), and "Free up space" is disabled too.
    void wholeSelectionPinnedByAnAncestorDisablesBothActions()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/sub/a.bin"), "hydrated"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/sub/b.bin"), "hydrated"));
        QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive/sub"))));

        const QList<QAction *> actions =
            createPlugin()->actions(selection({tree.path(QStringLiteral("OneDrive/sub/a.bin")), tree.path(QStringLiteral("OneDrive/sub/b.bin"))}), nullptr);
        QAction *alwaysKeep = find(actions, AlwaysKeep);
        QVERIFY(alwaysKeep);
        QVERIFY(alwaysKeep->isChecked());
        QVERIFY(!alwaysKeep->isEnabled());
        QVERIFY2(alwaysKeep->toolTip().contains(QStringLiteral("sub")), qPrintable(alwaysKeep->toolTip()));
        QAction *freeUp = find(actions, FreeUp);
        QVERIFY(freeUp);
        QVERIFY(!freeUp->isEnabled());
    }

    /// One item explicitly pinned and another pinned only by an ancestor:
    /// "Always keep" is disabled even though one of the two is explicitly
    /// pinned -- unchecking it would still refuse the whole call, since the
    /// ancestor-pinned one stays pinned by its ancestor either way
    /// (pinning.md §5) -- and "Free up space" is disabled too.
    void oneExplicitAndOneAncestorPinDisablesBothActions()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/explicit.bin"), "hydrated"));
        QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive/explicit.bin"))));
        QVERIFY(tree.file(QStringLiteral("OneDrive/sub/byAncestor.bin"), "hydrated"));
        QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive/sub"))));

        const QList<QAction *> actions = createPlugin()->actions(
            selection({tree.path(QStringLiteral("OneDrive/explicit.bin")), tree.path(QStringLiteral("OneDrive/sub/byAncestor.bin"))}), nullptr);
        QAction *alwaysKeep = find(actions, AlwaysKeep);
        QVERIFY(alwaysKeep);
        QVERIFY(alwaysKeep->isChecked());
        QVERIFY(!alwaysKeep->isEnabled());
        QAction *freeUp = find(actions, FreeUp);
        QVERIFY(freeUp);
        QVERIFY(!freeUp->isEnabled());
    }

    /// A path both explicitly pinned and pinned by an ancestor: unlike the
    /// mixed-selection case above, "Always keep" here is unchecked (not
    /// every item is explicitly this one -- there is only one item, and it
    /// is effectively pinned, so it IS checked) and disabled, since Unpin
    /// would still refuse it.
    void explicitAndAncestorPinOnTheSameItemDisablesAlwaysKeep()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/sub/both.bin"), "hydrated"));
        QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive/sub/both.bin"))));
        QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive/sub"))));

        const QList<QAction *> actions = createPlugin()->actions(selection({tree.path(QStringLiteral("OneDrive/sub/both.bin"))}), nullptr);
        QAction *alwaysKeep = find(actions, AlwaysKeep);
        QVERIFY(alwaysKeep);
        QVERIFY(alwaysKeep->isChecked());
        QVERIFY(!alwaysKeep->isEnabled());
        QAction *freeUp = find(actions, FreeUp);
        QVERIFY(freeUp);
        QVERIFY(!freeUp->isEnabled());
    }

    /// Folders get "Always keep" too, and "Free up space" for any folder in
    /// the root (D-B), pinned or not.
    void foldersGetActionsToo()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.dir(QStringLiteral("OneDrive/folder")));
        const KFileItem folder(url(tree.path(QStringLiteral("OneDrive/folder"))), QStringLiteral("inode/directory"), S_IFDIR);

        QList<QAction *> actions = createPlugin()->actions(KFileItemListProperties({folder}), nullptr);
        QAction *alwaysKeep = find(actions, AlwaysKeep);
        QVERIFY(alwaysKeep);
        QVERIFY(!alwaysKeep->isChecked());
        QVERIFY(find(actions, FreeUp));

        QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive/folder"))));
        actions = createPlugin()->actions(KFileItemListProperties({folder}), nullptr);
        QVERIFY(find(actions, AlwaysKeep)->isChecked());
        QVERIFY(find(actions, FreeUp));
    }

    /// Neither a symbolic link nor anything outside a root is offered
    /// anything.
    void nothingForALinkOrOutsideARoot()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        QVERIFY(tree.symlink(QStringLiteral("OneDrive/doc.bin"), QStringLiteral("OneDrive/link.bin")));
        QVERIFY(tree.file(QStringLiteral("Elsewhere/f.bin"), "online-only"));

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QCOMPARE(names(plugin->actions(selection({tree.path(QStringLiteral("OneDrive/link.bin"))}), nullptr)), QStringList());
        QCOMPARE(names(plugin->actions(selection({tree.path(QStringLiteral("Elsewhere/f.bin"))}), nullptr)), QStringList());
    }

    /// An unmanaged file (one of the user's own, in the sync folder), an
    /// unrecognised state, and a reserved `.konedrive-*` name are all left
    /// out of what is sent -- one of them must not make the daemon refuse
    /// the whole batch (review #7).
    void unmanagedUnrecognisedAndReservedAreLeftOut()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/mine.txt")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/odd.bin"), "not-a-state-we-know"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/.konedrive-tmp")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/good.bin"), "hydrated"));
        const auto p = [&tree](const char *name) {
            return tree.path(QString::fromLatin1(name));
        };
        QVERIFY(startFake());

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        const QStringList all{p("OneDrive/mine.txt"), p("OneDrive/odd.bin"), p("OneDrive/.konedrive-tmp"), p("OneDrive/good.bin")};
        find(plugin->actions(selection(all), nullptr), FreeUp)->trigger();
        QTRY_COMPARE(m_fake->calls, QStringList{QStringLiteral("FreeUp ") + p("OneDrive/good.bin")});
    }

    void nothingThroughALinkOutOfTheRoot_data()
    {
        QTest::addColumn<QString>("shownAs");
        QTest::newRow("through a link in the root to a folder outside") << QStringLiteral("OneDrive/escape/f.bin");
        QTest::newRow("through .. out of the root") << QStringLiteral("OneDrive/../Elsewhere/f.bin");
    }

    /// A placeholder outside the sync folder, reached through a symbolic
    /// link inside it, is not offered anything: it is not in the folder.
    void nothingThroughALinkOutOfTheRoot()
    {
        QFETCH(QString, shownAs);
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("Elsewhere/f.bin"), "online-only"));
        QVERIFY(tree.symlink(QStringLiteral("Elsewhere"), QStringLiteral("OneDrive/escape")));
        QCOMPARE(names(createPlugin()->actions(selection({tree.path(shownAs)}), nullptr)), QStringList());
    }

    /// The menu KFileItemActions builds -- the one Dolphin shows -- offers
    /// "Always keep" for anything inside a root, and "Free up space" for a
    /// folder in the selection (D-B), or when something in it is hydrated
    /// or explicitly pinned.
    void inTheContextMenuKioBuilds_data()
    {
        QTest::addColumn<QStringList>("entries"); // name:mimetype:state, or name/ for a folder
        QTest::addColumn<QStringList>("expected");
        QTest::newRow("one online-only file") << QStringList{QStringLiteral("notes.txt:text/plain:online-only")} << QStringList{AlwaysKeep};
        QTest::newRow("files of different types")
            << QStringList{QStringLiteral("notes.txt:text/plain:online-only"),
                           QStringLiteral("photo.jpg:image/jpeg:online-only"),
                           QStringLiteral("report.pdf:application/pdf:hydrated")}
            << QStringList{AlwaysKeep, FreeUp};
        QTest::newRow("a file and a folder") << QStringList{QStringLiteral("notes.txt:text/plain:online-only"), QStringLiteral("sub/")} << QStringList{AlwaysKeep, FreeUp};
        QTest::newRow("a folder alone") << QStringList{QStringLiteral("sub/")} << QStringList{AlwaysKeep, FreeUp};
        QTest::newRow("two folders") << QStringList{QStringLiteral("sub1/"), QStringLiteral("sub2/")} << QStringList{AlwaysKeep, FreeUp};
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

    /// Triggering an action calls Pin or FreeUp with every selected path
    /// that lies inside a root, whatever each one's own state is -- the
    /// daemon sorts out what each path needs.
    void triggeringCallsPinOrFreeUpWithTheWholeSelection()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/a.bin"), "online-only"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/b.bin"), "hydrated"));
        QVERIFY(tree.file(QStringLiteral("Elsewhere/c.bin"), "online-only"));
        const auto p = [&tree](const char *name) {
            return tree.path(QString::fromLatin1(name));
        };
        QVERIFY(startFake());

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        const QStringList all{p("OneDrive/a.bin"), p("OneDrive/b.bin"), p("Elsewhere/c.bin")};
        find(plugin->actions(selection(all), nullptr), AlwaysKeep)->trigger();
        QTRY_COMPARE(m_fake->calls, QStringList{QStringLiteral("Pin ") + p("OneDrive/a.bin") + QLatin1Char(',') + p("OneDrive/b.bin")});

        find(plugin->actions(selection(all), nullptr), FreeUp)->trigger();
        QTRY_COMPARE(m_fake->calls.size(), 2);
        QCOMPARE(m_fake->calls.at(1), QStringLiteral("FreeUp ") + p("OneDrive/a.bin") + QLatin1Char(',') + p("OneDrive/b.bin"));
    }

    /// D-A: unchecking an already-checked "Always keep on this device"
    /// calls Unpin, never FreeUp -- the files stay downloaded, as on
    /// Windows.
    void uncheckingAlwaysKeepCallsUnpinNotFreeUp()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/a.bin"), "hydrated"));
        QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive/a.bin"))));
        const QString a = tree.path(QStringLiteral("OneDrive/a.bin"));
        QVERIFY(startFake());

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QAction *alwaysKeep = find(plugin->actions(selection({a}), nullptr), AlwaysKeep);
        QVERIFY(alwaysKeep);
        QVERIFY(alwaysKeep->isChecked());
        alwaysKeep->trigger();
        QVERIFY(!alwaysKeep->isChecked());
        QTRY_COMPARE(m_fake->calls, QStringList{QStringLiteral("Unpin ") + a});
    }

    /// FreeUp's `busy` now also counts files changed here and not uploaded,
    /// not only files in use (review #6): a successful FreeUp with some
    /// still says so.
    void freeUpSuccessWithBusyFilesSaysSo()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/a.bin"), "hydrated"));
        const QString a = tree.path(QStringLiteral("OneDrive/a.bin"));
        QVERIFY(startFake());
        m_fake->defaultAnswer.files = 1;
        m_fake->defaultAnswer.busy = 2;

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection({a}), nullptr), FreeUp)->trigger();
        QTRY_COMPARE(errors.count(), 1);
        QVERIFY2(errors.at(0).at(0).toString().contains(QStringLiteral("2 files are in use or were changed here and were kept")),
                 qPrintable(errors.at(0).at(0).toString()));
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
        m_fake->defaultAnswer.delayMs = 2000;

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        QAction *alwaysKeep = find(plugin->actions(selection({doc}), nullptr), AlwaysKeep);
        QVERIFY(alwaysKeep);

        QElapsedTimer clock;
        clock.start();
        alwaysKeep->trigger();
        const qint64 took = clock.elapsed();
        QVERIFY2(took < 500, qPrintable(QStringLiteral("trigger() took %1 ms").arg(took)));

        QTRY_COMPARE(m_fake->calls, QStringList{QStringLiteral("Pin ") + doc});
        QCOMPARE(m_fake->delayedAnswersSent, 0);
        QTRY_COMPARE_WITH_TIMEOUT(m_fake->delayedAnswersSent, 1, 5000);
        QTest::qWait(konedrive::SyncClient::ReportDelayMs + 200);
        QCOMPARE(errors.count(), 0);
    }

    void refusalIsExplained_data()
    {
        QTest::addColumn<bool>("alwaysKeep");
        QTest::addColumn<QString>("errorName");
        QTest::addColumn<QString>("message");
        QTest::addColumn<QString>("expected");
        QTest::addColumn<bool>("daemonWordsShown");
        // False only for NotAllowed: the message is the daemon's own words
        // alone, naming the actual refused path -- not a file name of ours
        // in front of it, which could be the wrong path in the batch
        // (review #18).
        QTest::addColumn<bool>("fileNameShown");

        const QString decoy = QStringLiteral("DAEMON-OWN-WORDS");
        const auto named = [](const char *name) {
            return QStringLiteral("org.konedrive.Error.") + QLatin1String(name);
        };
        QTest::newRow("Always keep: NoHelper") << true << named("NoHelper") << decoy << QStringLiteral("must first have the helper stop letting its opens through unchecked") << false << true;
        QTest::newRow("Always keep: NoRoot") << true << named("NoRoot") << decoy << QStringLiteral("KOneDrive has no sync folder registered") << false << true;
        QTest::newRow("Always keep: OutsideRoot") << true << named("OutsideRoot") << decoy << QStringLiteral("is not inside KOneDrive's sync folder") << false << true;
        QTest::newRow("Always keep: NotManaged") << true << named("NotManaged") << decoy << QStringLiteral("nothing for KOneDrive to keep downloaded") << false << true;
        QTest::newRow("Always keep: ModifiedLocally") << true << named("ModifiedLocally") << decoy << QStringLiteral("downloading it again would overwrite your edits") << false << true;
        QTest::newRow("Always keep: Failed") << true << named("Failed") << QStringLiteral("disk full") << QStringLiteral("Keeping “doc.bin” on this device failed: disk full") << true << true;
        QTest::newRow("Always keep: a name that is not ours")
            << true << QStringLiteral("org.freedesktop.DBus.Error.InUse") << QStringLiteral("something else entirely")
            << QStringLiteral("Keeping “doc.bin” on this device failed: something else entirely") << true << true;
        QTest::newRow("Free up: NoHelper") << false << named("NoHelper") << decoy << QStringLiteral("must first have the helper take off any mark") << false << true;
        QTest::newRow("Free up: NoRoot") << false << named("NoRoot") << decoy << QStringLiteral("KOneDrive has no sync folder registered") << false << true;
        QTest::newRow("Free up: OutsideRoot") << false << named("OutsideRoot") << decoy << QStringLiteral("is not inside KOneDrive's sync folder") << false << true;
        QTest::newRow("Free up: NotManaged") << false << named("NotManaged") << decoy << QStringLiteral("never frees the space of a file it could not download again") << false << true;
        QTest::newRow("Free up: NotHydrated") << false << named("NotHydrated") << decoy << QStringLiteral("is not downloaded, so there is no space to free") << false << true;
        QTest::newRow("Free up: ModifiedLocally") << false << named("ModifiedLocally") << decoy << QStringLiteral("freeing its space would lose your edits") << false << true;
        QTest::newRow("Free up: InUse") << false << named("InUse") << decoy << QStringLiteral("is open in another program, so its space cannot be freed") << false << true;
        // The daemon's own shape: `<path> is pinned by <folder>: unpin it
        // first` -- the refused path first, which may not be "doc.bin".
        QTest::newRow("Free up: NotAllowed")
            << false << named("NotAllowed") << QStringLiteral("OneDrive/sub/byAncestor.bin is pinned by OneDrive/sub: unpin it first")
            << QStringLiteral("Could not free up space: OneDrive/sub/byAncestor.bin is pinned by OneDrive/sub: unpin it first") << true << false;
        QTest::newRow("Free up: Failed") << false << named("Failed") << QStringLiteral("disk full") << QStringLiteral("Freeing up “doc.bin” failed: disk full") << true << true;
    }

    /// Each refusal is explained by its name, in words about the user's file.
    void refusalIsExplained()
    {
        QFETCH(bool, alwaysKeep);
        QFETCH(QString, errorName);
        QFETCH(QString, message);
        QFETCH(QString, expected);
        QFETCH(bool, daemonWordsShown);
        QFETCH(bool, fileNameShown);
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), alwaysKeep ? "online-only" : "hydrated"));
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));
        QVERIFY(startFake());
        m_fake->defaultAnswer = {errorName, message, 0, 0, 0, 0, 0, 0};

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        QAction *action = find(plugin->actions(selection({doc}), nullptr), alwaysKeep ? AlwaysKeep : FreeUp);
        QVERIFY(action);
        action->trigger();
        QTRY_COMPARE(errors.count(), 1);
        const QString text = errors.at(0).at(0).toString();
        QVERIFY2(text.contains(expected), qPrintable(text));
        QCOMPARE(text.contains(QStringLiteral("“doc.bin”")), fileNameShown);
        QCOMPARE(text.contains(message), daemonWordsShown);
    }

    void daemonNotRunning_data()
    {
        QTest::addColumn<bool>("alwaysKeep");
        QTest::newRow("Always keep") << true;
        QTest::newRow("Free up space") << false;
    }

    /// With no daemon on the bus the action says so, plainly.
    void daemonNotRunning()
    {
        QFETCH(bool, alwaysKeep);
        QVERIFY(!QDBusConnection::sessionBus().interface()->isServiceRegistered(DaemonService));
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), alwaysKeep ? "online-only" : "hydrated"));

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        QAction *action = find(plugin->actions(selection({tree.path(QStringLiteral("OneDrive/doc.bin"))}), nullptr), alwaysKeep ? AlwaysKeep : FreeUp);
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
            QDBusMessage::createMethodCall(DaemonService, QStringLiteral("/org/konedrive/Daemon"), QStringLiteral("org.konedrive.Sync1"), QStringLiteral("Pin"))
            << QStringList{QStringLiteral("/nonexistent")});
        qInfo("the bus answers a failed start with %s", qPrintable(probe.errorName()));

        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection({tree.path(QStringLiteral("OneDrive/doc.bin"))}), nullptr), AlwaysKeep)->trigger();
        QTRY_COMPARE(errors.count(), 1);
        const QString text = errors.at(0).at(0).toString();
        QVERIFY2(text.startsWith(QStringLiteral("KOneDrive is not running, so “doc.bin” was not kept on this device")), qPrintable(text));
    }

    /// The daemon leaves the bus while a call waits for it.
    void daemonStopsBeforeAnswering()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));
        QVERIFY(startFake());
        m_fake->defaultAnswer.delayMs = 60000;

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection({doc}), nullptr), AlwaysKeep)->trigger();
        QTRY_COMPARE(m_fake->calls.size(), 1);
        m_fake->stop();
        QTRY_COMPARE(errors.count(), 1);
        const QString text = errors.at(0).at(0).toString();
        QVERIFY2(text.startsWith(QStringLiteral("KOneDrive stopped before it finished downloading everything of “doc.bin”")), qPrintable(text));
    }

    /// Three files refused together (one call, one reason) make one
    /// message, not three.
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
        find(plugin->actions(selection(paths), nullptr), AlwaysKeep)->trigger();
        QTRY_COMPARE(errors.count(), 1);
        QTest::qWait(3 * konedrive::SyncClient::ReportDelayMs);
        QCOMPARE(errors.count(), 1);
        const QString text = errors.at(0).at(0).toString();
        QVERIFY2(text.startsWith(QStringLiteral("KOneDrive is not running, so “a.bin” was not kept on this device")), qPrintable(text));
        QVERIFY2(text.contains(QStringLiteral("2 more files were not kept on this device for the same reason.")), qPrintable(text));
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
        m_fake->defaultAnswer = {QStringLiteral("org.konedrive.Error.Failed"), QStringLiteral("disk full"), 0, 0, 0, 0, 0, 0};

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection(paths), nullptr), AlwaysKeep)->trigger();
        QTRY_COMPARE(errors.count(), 1);
        QCOMPARE(errors.at(0).at(0).toString(),
                 QStringLiteral("Keeping “a.bin” on this device failed: disk full. One more file was not kept on this device for the same reason."));
    }

    /// A path whose call the daemon has not answered yet is not asked for
    /// again -- repeated clicks on a daemon that hangs would otherwise pile
    /// up calls in Dolphin -- and the user is told why nothing happened.
    void pathAlreadyWaitingIsNotAskedAgain()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/new.bin"), "online-only"));
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));
        const QString fresh = tree.path(QStringLiteral("OneDrive/new.bin"));
        QVERIFY(startFake());
        m_fake->defaultAnswer.delayMs = -1;

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection({doc}), nullptr), AlwaysKeep)->trigger();
        QTRY_COMPARE(m_fake->calls, QStringList{QStringLiteral("Pin ") + doc});

        find(plugin->actions(selection({doc}), nullptr), AlwaysKeep)->trigger();
        QTest::qWait(konedrive::SyncClient::ReportDelayMs + 200);
        QCOMPARE(m_fake->calls.size(), 1);
        QTRY_COMPARE(errors.count(), 1);
        QVERIFY2(errors.at(0).at(0).toString().startsWith(QStringLiteral("KOneDrive has not yet answered an earlier request for “doc.bin”, so it was not asked again.")),
                 qPrintable(errors.at(0).at(0).toString()));

        // With a path not asked for yet: that one is sent, the other is not.
        find(plugin->actions(selection({doc, fresh}), nullptr), AlwaysKeep)->trigger();
        QTRY_COMPARE(errors.count(), 2);
        QVERIFY2(!errors.at(1).at(0).toString().contains(QStringLiteral("“new.bin”")), qPrintable(errors.at(1).at(0).toString()));
        QTest::qWait(konedrive::SyncClient::ReportDelayMs + 200);
        QCOMPARE(m_fake->calls.size(), 2);
        QCOMPARE(m_fake->calls.at(1), QStringLiteral("Pin ") + fresh);
    }

    /// However many paths are chosen, no more than MaxCallsInFlight wait for
    /// the daemon at once: they cost Dolphin memory, and on a dbus-daemon
    /// bus they share Dolphin's own budget of pending replies. The rest are
    /// refused with a message that says so, and are not part of the one
    /// call that is made.
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
        m_fake->defaultAnswer.delayMs = -1;

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection(paths), nullptr), AlwaysKeep)->trigger();
        QTRY_COMPARE(m_fake->calls.size(), 1);
        const QStringList sent = m_fake->calls.first().mid(QStringLiteral("Pin ").size()).split(QLatin1Char(','));
        QCOMPARE(sent.size(), cap);
        QVERIFY(!sent.contains(paths.last()));
        QTRY_COMPARE(errors.count(), 1);
        const QString text = errors.at(0).at(0).toString();
        // The count is written the locale's way ("1,000" here).
        QVERIFY2(QString(text).remove(QLatin1Char(',')).startsWith(QString::number(cap) + QLatin1Char(' ')), qPrintable(text));
        QVERIFY2(text.contains(QStringLiteral(" requests to KOneDrive are already waiting for an answer, so “f%1.bin” was not kept on this device.")
                                   .arg(cap, 5, 10, QLatin1Char('0'))),
                 qPrintable(text));
    }

    /// Freeing up (or keeping) something that takes longer than D-Bus's
    /// default 25-second reply timeout is not reported as a failure.
    void keepingOnThisDeviceLongerThanTheDefaultTimeoutIsNotAFailure()
    {
        if (qEnvironmentVariableIsEmpty("KONEDRIVE_TEST_SLOW")) {
            QSKIP("takes 30 s; ctest runs it as actionplugintest_slow");
        }
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));
        QVERIFY(startFake());
        m_fake->defaultAnswer.delayMs = 30000;

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection({doc}), nullptr), AlwaysKeep)->trigger();
        QTRY_COMPARE_WITH_TIMEOUT(m_fake->delayedAnswersSent, 1, 40000);
        QTest::qWait(konedrive::SyncClient::ReportDelayMs + 500);
        QVERIFY2(errors.isEmpty(), errors.isEmpty() ? "" : qPrintable(errors.at(0).at(0).toString()));
    }
};

QTEST_MAIN(ActionPluginTest)

#include "actionplugintest.moc"
