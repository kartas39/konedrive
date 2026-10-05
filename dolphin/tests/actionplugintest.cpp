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
const QString Section = QStringLiteral("konedrive_section");
const QString OpenOnline = QStringLiteral("konedrive_open_online");
const QString SectionEnd = QStringLiteral("konedrive_section_end");
const QString DaemonService = QStringLiteral("org.konedrive.Daemon");
} // namespace

class ActionPluginTest : public QObject
{
    Q_OBJECT

    std::unique_ptr<FakeSync> m_fake;
    /// Takes every address the plugin opens: no test starts a browser.
    std::unique_ptr<UrlCatcher> m_urls;

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
        m_urls = std::make_unique<UrlCatcher>();
        QVERIFY(createPlugin());
    }

    void cleanupTestCase()
    {
        m_urls.reset();
    }

    void cleanup()
    {
        m_fake.reset();
        m_urls->opened.clear();
    }

    void theEntriesAreWhatTheDaemonAnswers_data()
    {
        QTest::addColumn<bool>("taken"); // whether `paths` of the answer holds the selected file
        QTest::addColumn<QString>("alwaysKeep");
        QTest::addColumn<QString>("freeUp");
        QTest::addColumn<QString>("blockedBy");
        QTest::addColumn<QString>("openOnline");
        QTest::addColumn<QStringList>("expected");
        QTest::addColumn<bool>("keepChecked");
        QTest::addColumn<QString>("keepToolTip"); // not empty: disabled, with these words
        QTest::addColumn<bool>("freeUpEnabled");
        QTest::addColumn<QString>("freeUpToolTip");
        QTest::addColumn<bool>("openOnlineEnabled");

        const QString hidden = QStringLiteral("hidden");
        const QString enabled = QStringLiteral("enabled");
        const QString disabled = QStringLiteral("disabled");
        const QString none;
        QTest::newRow("nothing") << false << hidden << hidden << none << hidden << QStringList() << false << none << false << none << false;
        QTest::newRow("always keep, unchecked") << true << QStringLiteral("off") << hidden << none << hidden << QStringList{Section, AlwaysKeep, SectionEnd} << false << none
                                                << false << none << false;
        QTest::newRow("always keep checked, free up, open") << true << QStringLiteral("on") << enabled << none << enabled
                                                            << QStringList{Section, AlwaysKeep, FreeUp, OpenOnline, SectionEnd} << true << none << true << none << true;
        QTest::newRow("kept by a folder above") << true << QStringLiteral("on-locked") << disabled << QStringLiteral("sub") << hidden
                                                << QStringList{Section, AlwaysKeep, FreeUp, SectionEnd} << true << QStringLiteral("Kept on this device because “sub” is.") << false
                                                << QStringLiteral("Kept on this device because “sub” is; unpin it first.") << false;
        QTest::newRow("unchecked, free up kept by a folder above")
            << true << QStringLiteral("off") << disabled << QStringLiteral("sub") << hidden << QStringList{Section, AlwaysKeep, FreeUp, SectionEnd} << false << none << false
            << QStringLiteral("Kept on this device because “sub” is; unpin it first.") << false;
        QTest::newRow("free up refused for another reason") << true << QStringLiteral("off") << disabled << none << hidden << QStringList{Section, AlwaysKeep, FreeUp, SectionEnd}
                                                            << false << none << false << none << false;
        QTest::newRow("not in OneDrive yet") << true << QStringLiteral("off") << hidden << none << disabled << QStringList{Section, AlwaysKeep, OpenOnline, SectionEnd} << false
                                             << none << false << none << false;
        QTest::newRow("open in OneDrive alone") << false << hidden << hidden << none << enabled << QStringList{Section, OpenOnline, SectionEnd} << false << none << false << none
                                                << true;
        QTest::newRow("values this plugin does not know") << true << QStringLiteral("sideways") << QStringLiteral("perhaps") << none << QStringLiteral("later") << QStringList()
                                                          << false << none << false << none << false;
        // Entries with nothing to call the daemon with are not shown.
        QTest::newRow("entries, and no paths for them") << false << QStringLiteral("on") << enabled << none << hidden << QStringList() << false << none << false << none << false;
    }

    /// The plugin shows what Menu answers, each value of each key, and
    /// decides nothing from the marks: the file here is online-only and
    /// carries no pin, whatever the answer says.
    void theEntriesAreWhatTheDaemonAnswers()
    {
        QFETCH(bool, taken);
        QFETCH(QString, alwaysKeep);
        QFETCH(QString, freeUp);
        QFETCH(QString, blockedBy);
        QFETCH(QString, openOnline);
        QFETCH(QStringList, expected);
        QFETCH(bool, keepChecked);
        QFETCH(QString, keepToolTip);
        QFETCH(bool, freeUpEnabled);
        QFETCH(QString, freeUpToolTip);
        QFETCH(bool, openOnlineEnabled);

        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));
        QVERIFY(startFake());
        m_fake->menuAnswer = FakeSync::menuOf(taken ? QStringList{doc} : QStringList(), alwaysKeep, freeUp, blockedBy, openOnline, doc);

        const QList<QAction *> actions = createPlugin()->actions(selection({doc}), nullptr);
        QCOMPARE(names(actions), expected);
        QCOMPARE(m_fake->menuCalls(), QList<QStringList>{{doc}});
        if (QAction *keep = find(actions, AlwaysKeep)) {
            QCOMPARE(keep->text(), QStringLiteral("Always Keep on This Device"));
            QVERIFY(keep->isCheckable());
            QCOMPARE(keep->isChecked(), keepChecked);
            QCOMPARE(keep->isEnabled(), keepToolTip.isEmpty());
            if (!keepToolTip.isEmpty()) {
                QCOMPARE(keep->toolTip(), keepToolTip);
            }
        }
        if (QAction *free = find(actions, FreeUp)) {
            QCOMPARE(free->text(), QStringLiteral("Free Up Space"));
            QCOMPARE(free->isEnabled(), freeUpEnabled);
            if (!freeUpEnabled) {
                // With no folder to name there are no words of the plugin's
                // own: Qt then shows the entry's text.
                QCOMPARE(free->toolTip(), freeUpToolTip.isEmpty() ? QStringLiteral("Free Up Space") : freeUpToolTip);
            }
        }
        if (QAction *online = find(actions, OpenOnline)) {
            QCOMPARE(online->text(), QStringLiteral("Open in OneDrive"));
            QCOMPARE(online->isEnabled(), openOnlineEnabled);
            if (!openOnlineEnabled) {
                QCOMPARE(online->toolTip(), QStringLiteral("Not in OneDrive yet."));
            }
        }
    }

    void noAnswerGivesNoEntries_data()
    {
        QTest::addColumn<QString>("daemon");
        QTest::newRow("is not running") << QStringLiteral("none");
        QTest::newRow("never answers") << QStringLiteral("silent");
        QTest::newRow("answers too late") << QStringLiteral("late");
        QTest::newRow("answers with an error") << QStringLiteral("error");
    }

    /// With no answer -- no daemon, none in time, a refusal -- KOneDrive has
    /// no entries in the menu: nothing is decided from the marks instead.
    /// The menu is never held up for longer than the timeout.
    void noAnswerGivesNoEntries()
    {
        QFETCH(QString, daemon);
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "hydrated"));
        QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive/doc.bin"))));
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));
        if (daemon != QLatin1String("none")) {
            QVERIFY(startFake());
            m_fake->menuDelayMs = daemon == QLatin1String("silent") ? -1 : daemon == QLatin1String("late") ? 4 * konedrive::SyncClient::MenuTimeoutMs : 0;
            if (daemon == QLatin1String("error")) {
                m_fake->menuErrorName = QStringLiteral("org.konedrive.Error.Failed");
            }
        }

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QElapsedTimer clock;
        clock.start();
        QCOMPARE(names(plugin->actions(selection({doc}), nullptr)), QStringList());
        const qint64 took = clock.elapsed();
        QVERIFY2(took < 4 * konedrive::SyncClient::MenuTimeoutMs, qPrintable(QStringLiteral("actions() took %1 ms").arg(took)));
        if (m_fake) {
            QTRY_COMPARE(m_fake->menuCalls(), QList<QStringList>{{doc}});
            QCOMPARE(m_fake->calls(), QStringList());
        } else {
            QVERIFY(!QDBusConnection::sessionBus().interface()->isServiceRegistered(DaemonService));
        }
    }

    /// By the marks alone, before anything is asked: a selection with
    /// nothing in a sync folder makes no call, and has no entries.
    void aSelectionOutsideEverySyncFolderMakesNoCall()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        QVERIFY(tree.file(QStringLiteral("Elsewhere/f.bin"), "online-only"));
        QVERIFY(tree.dir(QStringLiteral("Elsewhere/folder")));
        // A placeholder outside the sync folder, reached through a symbolic
        // link inside it or through `..`, is not in the folder.
        QVERIFY(tree.symlink(QStringLiteral("Elsewhere"), QStringLiteral("OneDrive/escape")));
        const auto p = [&tree](const char *name) {
            return tree.path(QString::fromLatin1(name));
        };
        QVERIFY(startFake());

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        for (const QStringList &outside : {QStringList{p("Elsewhere/f.bin")},
                                           QStringList{p("Elsewhere/folder")},
                                           QStringList{p("OneDrive/escape/f.bin")},
                                           QStringList{p("OneDrive/../Elsewhere/f.bin")},
                                           QStringList{p("Elsewhere/f.bin"), p("Elsewhere/folder")}}) {
            QCOMPARE(names(plugin->actions(selection(outside), nullptr)), QStringList());
        }
        QCOMPARE(m_fake->menuCalls(), QList<QStringList>());

        // One path in a sync folder is enough, and the daemon is asked about
        // the whole selection, as it was given; so is an account's folder
        // itself.
        const QStringList mixed{p("Elsewhere/f.bin"), p("OneDrive/doc.bin")};
        QVERIFY(!plugin->actions(selection(mixed), nullptr).isEmpty());
        QVERIFY(!plugin->actions(selection({p("OneDrive")}), nullptr).isEmpty());
        QCOMPARE(m_fake->menuCalls(), (QList<QStringList>{mixed, {p("OneDrive")}}));
        QCOMPARE(m_fake->calls(), QStringList());
    }

    /// The menu KFileItemActions builds -- the one Dolphin shows -- holds
    /// KOneDrive's entries as one section: the heading, what the daemon
    /// answered, the closing separator.
    void inTheContextMenuKioBuilds_data()
    {
        QTest::addColumn<QStringList>("entries"); // name:mimetype:state, or name/ for a folder
        QTest::addColumn<QStringList>("expected");
        QTest::newRow("one file") << QStringList{QStringLiteral("notes.txt:text/plain:online-only")} << QStringList{Section, AlwaysKeep, FreeUp, OpenOnline, SectionEnd};
        QTest::newRow("files of different types")
            << QStringList{QStringLiteral("notes.txt:text/plain:online-only"),
                           QStringLiteral("photo.jpg:image/jpeg:online-only"),
                           QStringLiteral("report.pdf:application/pdf:hydrated")}
            << QStringList{Section, AlwaysKeep, FreeUp, SectionEnd};
        QTest::newRow("a file and a folder") << QStringList{QStringLiteral("notes.txt:text/plain:online-only"), QStringLiteral("sub/")}
                                             << QStringList{Section, AlwaysKeep, FreeUp, SectionEnd};
        QTest::newRow("a folder alone") << QStringList{QStringLiteral("sub/")} << QStringList{Section, AlwaysKeep, FreeUp, OpenOnline, SectionEnd};
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
        // The stand-in takes every path, and offers "Open in OneDrive" for one.
        QVERIFY(startFake());

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
        // In the order the menu shows them: the heading first, the closing
        // separator last, the entries between them.
        QStringList shown;
        const QList<QAction *> actions = menu.actions();
        for (const QAction *action : actions) {
            if (action->objectName().startsWith(QLatin1String("konedrive_"))) {
                shown.append(action->objectName());
            }
        }
        QCOMPARE(shown, expected);
        QAction *heading = menu.findChild<QAction *>(Section);
        if (!heading) {
            heading = find(actions, Section);
        }
        QVERIFY(heading);
        QVERIFY(heading->isSeparator());
        QCOMPARE(heading->text(), QStringLiteral("OneDrive"));
        QVERIFY(find(actions, SectionEnd)->isSeparator());
    }

    /// The account's folder itself, as the daemon answers about it: the
    /// heading, "Open in OneDrive" and the closing separator, nothing else.
    /// A click asks for the path the answer named.
    void theAccountsFolderItselfOpensInOneDrive()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        const QString root = tree.path(QStringLiteral("OneDrive"));
        const KFileItem folder(url(root), QStringLiteral("inode/directory"), S_IFDIR);
        QVERIFY(startFake());
        m_fake->webUrl = QStringLiteral("https://onedrive.example/root");
        m_fake->menuAnswer = FakeSync::menuOf({}, QStringLiteral("hidden"), QStringLiteral("hidden"), QString(), QStringLiteral("enabled"), root);

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        const QList<QAction *> actions = plugin->actions(KFileItemListProperties({folder}), nullptr);
        QCOMPARE(names(actions), (QStringList{Section, OpenOnline, SectionEnd}));
        QVERIFY(find(actions, OpenOnline)->isEnabled());
        find(actions, OpenOnline)->trigger();
        QTRY_COMPARE(m_urls->opened, QList<QUrl>{QUrl(QStringLiteral("https://onedrive.example/root"))});
        QCOMPARE(m_fake->calls(), QStringList{QStringLiteral("WebUrl ") + root});
    }

    /// A click makes one WebUrl call with the path, and the address that
    /// comes back is opened -- here, handed to the test's own handler.
    void openInOneDriveOpensTheAddressTheDaemonAnswers()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));
        QVERIFY(setItemId(doc));
        QVERIFY(startFake());
        m_fake->webUrl = QStringLiteral("https://onedrive.example/doc?id=1");

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection({doc}), nullptr), OpenOnline)->trigger();
        QTRY_COMPARE(m_urls->opened, QList<QUrl>{QUrl(QStringLiteral("https://onedrive.example/doc?id=1"))});
        QCOMPARE(m_fake->calls(), QStringList{QStringLiteral("WebUrl ") + doc});
        QTest::qWait(konedrive::SyncClient::ReportDelayMs + 200);
        QCOMPARE(errors.count(), 0);

        // An answer that is not an address of the web is opened by nothing.
        m_fake->webUrl = QStringLiteral("file:///etc/passwd");
        find(plugin->actions(selection({doc}), nullptr), OpenOnline)->trigger();
        QTRY_COMPARE(errors.count(), 1);
        QVERIFY2(errors.at(0).at(0).toString().contains(QStringLiteral("The page of “doc.bin” in OneDrive could not be opened")), qPrintable(errors.at(0).at(0).toString()));
        QCOMPARE(m_urls->opened.size(), 1);
    }

    void openInOneDriveRefusalIsExplained_data()
    {
        QTest::addColumn<QString>("errorName");
        QTest::addColumn<QString>("message");
        QTest::addColumn<QString>("expected");
        const QString decoy = QStringLiteral("DAEMON-OWN-WORDS");
        const auto named = [](const char *name) {
            return QStringLiteral("org.konedrive.Error.") + QLatin1String(name);
        };
        QTest::newRow("NotUploaded") << named("NotUploaded") << decoy << QStringLiteral("“doc.bin” is not uploaded yet, so it has no page in OneDrive.");
        QTest::newRow("Unreachable, with the cause") << named("Unreachable") << QStringLiteral("the secret storage is locked")
                                                     << QStringLiteral("OneDrive could not be reached: the secret storage is locked");
        QTest::newRow("Unreachable, no message") << named("Unreachable") << QString() << QStringLiteral("OneDrive could not be reached.");
        QTest::newRow("NotSignedIn") << named("NotSignedIn") << decoy
                                     << QStringLiteral("The account is not signed in, so OneDrive cannot be asked for the page of “doc.bin”. Sign in and try again.");
        QTest::newRow("OutsideRoot") << named("OutsideRoot") << decoy << QStringLiteral("“doc.bin” is not inside any of KOneDrive's folders, so it has no page in OneDrive.");
        QTest::newRow("NotManaged") << named("NotManaged") << decoy
                                    << QStringLiteral("“doc.bin” is not a OneDrive file: it is a file of your own in the sync folder, so it has no page in OneDrive.");
        QTest::newRow("NoRoot") << named("NoRoot") << decoy << QStringLiteral("The folder holding “doc.bin” is no longer registered with KOneDrive, so nothing was done with it.");
        QTest::newRow("Failed") << named("Failed") << QStringLiteral("doc.bin is not in OneDrive any more")
                                << QStringLiteral("Opening “doc.bin” in OneDrive failed: doc.bin is not in OneDrive any more");
    }

    /// Each refusal of WebUrl is explained by its name, and nothing is opened.
    void openInOneDriveRefusalIsExplained()
    {
        QFETCH(QString, errorName);
        QFETCH(QString, message);
        QFETCH(QString, expected);
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "hydrated"));
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));
        QVERIFY(setItemId(doc));
        QVERIFY(startFake());
        m_fake->defaultAnswer = {errorName, message, 0, 0, 0, 0, 0, 0};

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        find(plugin->actions(selection({doc}), nullptr), OpenOnline)->trigger();
        QTRY_COMPARE(errors.count(), 1);
        QCOMPARE(errors.at(0).at(0).toString(), expected);
        QVERIFY(m_urls->opened.isEmpty());
    }

    /// With no daemon, and with one that stops before answering, "Open in
    /// OneDrive" says so as the other entries do; a path still waiting is
    /// not asked again.
    void openInOneDriveFollowsTheRulesOfTheOtherCalls()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "hydrated"));
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));
        QVERIFY(setItemId(doc));

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        // The menu was built while the daemon ran; it is gone by the click.
        QVERIFY(startFake());
        QAction *open = find(plugin->actions(selection({doc}), nullptr), OpenOnline);
        QVERIFY(open);
        m_fake.reset();
        open->trigger();
        QTRY_COMPARE(errors.count(), 1);
        QVERIFY2(errors.at(0).at(0).toString().startsWith(QStringLiteral("KOneDrive is not running, so “doc.bin” was not opened in OneDrive.")),
                 qPrintable(errors.at(0).at(0).toString()));

        QVERIFY(startFake());
        m_fake->defaultAnswer.delayMs = -1;
        find(plugin->actions(selection({doc}), nullptr), OpenOnline)->trigger();
        QTRY_COMPARE(m_fake->calls().size(), 1);
        find(plugin->actions(selection({doc}), nullptr), OpenOnline)->trigger();
        QTRY_COMPARE(errors.count(), 2);
        QVERIFY2(errors.at(1).at(0).toString().startsWith(QStringLiteral("KOneDrive has not yet answered an earlier request for “doc.bin”")),
                 qPrintable(errors.at(1).at(0).toString()));
        QCOMPARE(m_fake->calls().size(), 1);

        m_fake->stop();
        QTRY_COMPARE(errors.count(), 3);
        QVERIFY2(errors.at(2).at(0).toString().startsWith(QStringLiteral("KOneDrive stopped before it found the page of “doc.bin” in OneDrive.")),
                 qPrintable(errors.at(2).at(0).toString()));
        QVERIFY(m_urls->opened.isEmpty());
    }

    /// Triggering an action calls Pin or FreeUp with the paths the daemon
    /// answered -- those it takes, not the selection: one path it refuses
    /// must not make it refuse the whole batch.
    void triggeringCallsPinOrFreeUpWithThePathsOfTheAnswer()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/a.bin"), "online-only"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/b.bin"), "hydrated"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/mine.txt")));
        QVERIFY(tree.file(QStringLiteral("Elsewhere/c.bin"), "online-only"));
        const auto p = [&tree](const char *name) {
            return tree.path(QString::fromLatin1(name));
        };
        QVERIFY(startFake());
        const QStringList taken{p("OneDrive/a.bin"), p("OneDrive/b.bin")};
        m_fake->menuAnswer = FakeSync::menuOf(taken, QStringLiteral("off"), QStringLiteral("enabled"));

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        const QStringList all{p("OneDrive/a.bin"), p("OneDrive/mine.txt"), p("OneDrive/b.bin"), p("Elsewhere/c.bin")};
        find(plugin->actions(selection(all), nullptr), AlwaysKeep)->trigger();
        QTRY_COMPARE(m_fake->calls(), QStringList{QStringLiteral("Pin ") + taken.join(QLatin1Char(','))});

        find(plugin->actions(selection(all), nullptr), FreeUp)->trigger();
        QTRY_COMPARE(m_fake->calls().size(), 2);
        QCOMPARE(m_fake->calls().at(1), QStringLiteral("FreeUp ") + taken.join(QLatin1Char(',')));
        QCOMPARE(m_fake->menuCalls(), (QList<QStringList>{all, all}));
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
        m_fake->menuAnswer = FakeSync::menuOf({a}, QStringLiteral("on"), QStringLiteral("enabled"));

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QAction *alwaysKeep = find(plugin->actions(selection({a}), nullptr), AlwaysKeep);
        QVERIFY(alwaysKeep);
        QVERIFY(alwaysKeep->isChecked());
        alwaysKeep->trigger();
        QVERIFY(!alwaysKeep->isChecked());
        QTRY_COMPARE(m_fake->calls(), QStringList{QStringLiteral("Unpin ") + a});
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

        QTRY_COMPARE(m_fake->calls(), QStringList{QStringLiteral("Pin ") + doc});
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
        QTest::newRow("Always keep: NoRoot") << true << named("NoRoot") << decoy << QStringLiteral("is no longer registered with KOneDrive") << false << true;
        QTest::newRow("Always keep: OutsideRoot") << true << named("OutsideRoot") << decoy << QStringLiteral("is not inside any of KOneDrive's folders") << false << true;
        QTest::newRow("Always keep: NotManaged") << true << named("NotManaged") << decoy << QStringLiteral("nothing for KOneDrive to keep downloaded") << false << true;
        QTest::newRow("Always keep: ModifiedLocally") << true << named("ModifiedLocally") << decoy << QStringLiteral("downloading it again would overwrite your edits") << false << true;
        QTest::newRow("Always keep: Failed") << true << named("Failed") << QStringLiteral("disk full") << QStringLiteral("Keeping “doc.bin” on this device failed: disk full") << true << true;
        QTest::newRow("Always keep: a name that is not ours")
            << true << QStringLiteral("org.freedesktop.DBus.Error.InUse") << QStringLiteral("something else entirely")
            << QStringLiteral("Keeping “doc.bin” on this device failed: something else entirely") << true << true;
        QTest::newRow("Free up: NoHelper") << false << named("NoHelper") << decoy << QStringLiteral("must first have the helper take off any mark") << false << true;
        QTest::newRow("Free up: NoRoot") << false << named("NoRoot") << decoy << QStringLiteral("is no longer registered with KOneDrive") << false << true;
        QTest::newRow("Free up: OutsideRoot") << false << named("OutsideRoot") << decoy << QStringLiteral("is not inside any of KOneDrive's folders") << false << true;
        QTest::newRow("Free up: NotManaged") << false << named("NotManaged") << decoy << QStringLiteral("never frees the space of a file it could not download again") << false << true;
        QTest::newRow("Free up: NotHydrated") << false << named("NotHydrated") << decoy << QStringLiteral("is not downloaded, so there is no space to free") << false << true;
        QTest::newRow("Free up: ModifiedLocally") << false << named("ModifiedLocally") << decoy << QStringLiteral("freeing its space would lose your edits") << false << true;
        QTest::newRow("Free up: InUse") << false << named("InUse") << decoy << QStringLiteral("is open in another program, so its space cannot be freed") << false << true;
        QTest::newRow("Free up: NotUploaded") << false << named("NotUploaded") << decoy << QStringLiteral("is not uploaded yet, so freeing it up would lose the changes made here") << false << true;
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

    /// With no daemon on the bus by the time an entry is chosen, the action
    /// says so, plainly.
    void daemonNotRunning()
    {
        QFETCH(bool, alwaysKeep);
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), alwaysKeep ? "online-only" : "hydrated"));

        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        // The menu was built while the daemon ran; it is gone by the click.
        QVERIFY(startFake());
        QAction *action = find(plugin->actions(selection({tree.path(QStringLiteral("OneDrive/doc.bin"))}), nullptr), alwaysKeep ? AlwaysKeep : FreeUp);
        QVERIFY(action);
        m_fake.reset();
        QVERIFY(!QDBusConnection::sessionBus().interface()->isServiceRegistered(DaemonService));
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
            QDBusMessage::createMethodCall(DaemonService, QStringLiteral("/org/konedrive/Accounts"), QStringLiteral("org.konedrive.Files"), QStringLiteral("Pin"))
            << QStringList{QStringLiteral("/nonexistent")});
        qInfo("the bus answers a failed start with %s", qPrintable(probe.errorName()));

        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        KAbstractFileItemActionPlugin *plugin = createPlugin();
        QSignalSpy errors(plugin, &KAbstractFileItemActionPlugin::error);
        // Building a menu never starts the daemon: with none running there
        // are no entries, and nothing was asked to start.
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));
        QCOMPARE(names(plugin->actions(selection({doc}), nullptr)), QStringList());
        QVERIFY(!QDBusConnection::sessionBus().interface()->isServiceRegistered(DaemonService));
        // Choosing an entry of a menu built while it ran does ask for it.
        QVERIFY(startFake());
        QAction *alwaysKeep = find(plugin->actions(selection({doc}), nullptr), AlwaysKeep);
        QVERIFY(alwaysKeep);
        m_fake.reset();
        alwaysKeep->trigger();
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
        QTRY_COMPARE(m_fake->calls().size(), 1);
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
        // The menu was built while the daemon ran; it is gone by the click.
        QVERIFY(startFake());
        QAction *alwaysKeep = find(plugin->actions(selection(paths), nullptr), AlwaysKeep);
        QVERIFY(alwaysKeep);
        m_fake.reset();
        alwaysKeep->trigger();
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
        QTRY_COMPARE(m_fake->calls(), QStringList{QStringLiteral("Pin ") + doc});

        find(plugin->actions(selection({doc}), nullptr), AlwaysKeep)->trigger();
        QTest::qWait(konedrive::SyncClient::ReportDelayMs + 200);
        QCOMPARE(m_fake->calls().size(), 1);
        QTRY_COMPARE(errors.count(), 1);
        QVERIFY2(errors.at(0).at(0).toString().startsWith(QStringLiteral("KOneDrive has not yet answered an earlier request for “doc.bin”, so it was not asked again.")),
                 qPrintable(errors.at(0).at(0).toString()));

        // With a path not asked for yet: that one is sent, the other is not.
        find(plugin->actions(selection({doc, fresh}), nullptr), AlwaysKeep)->trigger();
        QTRY_COMPARE(errors.count(), 2);
        QVERIFY2(!errors.at(1).at(0).toString().contains(QStringLiteral("“new.bin”")), qPrintable(errors.at(1).at(0).toString()));
        QTest::qWait(konedrive::SyncClient::ReportDelayMs + 200);
        QCOMPARE(m_fake->calls().size(), 2);
        QCOMPARE(m_fake->calls().at(1), QStringLiteral("Pin ") + fresh);
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
        QTRY_COMPARE(m_fake->calls().size(), 1);
        const QStringList sent = m_fake->calls().first().mid(QStringLiteral("Pin ").size()).split(QLatin1Char(','));
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
