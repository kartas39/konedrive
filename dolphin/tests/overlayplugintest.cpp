// The overlay plugin as Dolphin loads and uses it: found under
// kf6/overlayicon, instantiated once through QPluginLoader, asked for each
// file's overlays, and listened to for overlaysChanged
// (dolphin src/kitemviews/kfileitemmodelrolesupdater.cpp).

#include "testsupport.h"

#include <KOverlayIconPlugin>
#include <KPluginMetaData>

#include <QDBusConnection>
#include <QDBusConnectionInterface>
#include <QFileInfo>
#include <QPluginLoader>
#include <QSignalSpy>
#include <QStandardPaths>
#include <QTest>

using namespace testsupport;

namespace
{
const QStringList Cloud{QStringLiteral("cloudstatus")};
const QStringList Syncing{QStringLiteral("state-sync")};
const QStringList CheckOutline{QStringLiteral("dialog-ok")};
const QStringList CheckFilled{QStringLiteral("emblem-checked")};
const QStringList Error{QStringLiteral("state-error")};
} // namespace

class OverlayPluginTest : public QObject
{
    Q_OBJECT

    KOverlayIconPlugin *m_plugin = nullptr;

    QStringList overlays(const QString &path)
    {
        return m_plugin->getOverlays(url(path));
    }

private Q_SLOTS:
    void initTestCase()
    {
        QStandardPaths::setTestModeEnabled(true);
        QPluginLoader loader(QStringLiteral(KONEDRIVE_OVERLAY_PLUGIN));
        m_plugin = qobject_cast<KOverlayIconPlugin *>(loader.instance());
        QVERIFY2(m_plugin, qPrintable(loader.errorString()));
    }

    /// Where and how Dolphin looks for overlay plugins.
    void foundWhereDolphinLooks()
    {
        const QList<KPluginMetaData> plugins = KPluginMetaData::findPlugins(QStringLiteral("kf6/overlayicon"), {}, KPluginMetaData::AllowEmptyMetaData);
        const QString built = QFileInfo(QStringLiteral(KONEDRIVE_OVERLAY_PLUGIN)).canonicalFilePath();
        KPluginMetaData ours;
        for (const KPluginMetaData &plugin : plugins) {
            if (QFileInfo(plugin.fileName()).canonicalFilePath() == built) {
                ours = plugin;
            }
        }
        QVERIFY2(ours.isValid(), "kf6/overlayicon does not list the built plugin");
        QCOMPARE(ours.pluginId(), QStringLiteral("konedriveoverlay"));
        QCOMPARE(qobject_cast<KOverlayIconPlugin *>(QPluginLoader(ours.fileName()).instance()), m_plugin);
    }

    void emblemForEachState_data()
    {
        QTest::addColumn<QByteArray>("state");
        QTest::addColumn<QString>("pin"); // "none", "explicit", "ancestor"
        QTest::addColumn<QStringList>("expected");
        QTest::newRow("online-only, not pinned: cloud") << QByteArray("online-only") << QStringLiteral("none") << Cloud;
        QTest::newRow("online-only, pinned: syncing (the sweep will fill it)") << QByteArray("online-only") << QStringLiteral("explicit") << Syncing;
        QTest::newRow("hydrating: syncing") << QByteArray("hydrating") << QStringLiteral("none") << Syncing;
        QTest::newRow("dehydrating: syncing") << QByteArray("dehydrating") << QStringLiteral("none") << Syncing;
        QTest::newRow("hydrated, not pinned: outline check") << QByteArray("hydrated") << QStringLiteral("none") << CheckOutline;
        QTest::newRow("hydrated, explicitly pinned: filled check") << QByteArray("hydrated") << QStringLiteral("explicit") << CheckFilled;
        QTest::newRow("hydrated, pinned by the root folder: filled check") << QByteArray("hydrated") << QStringLiteral("ancestor") << CheckFilled;
        QTest::newRow("no attribute: none") << QByteArray() << QStringLiteral("none") << QStringList();
        QTest::newRow("a value we do not know: none") << QByteArray("downloaded") << QStringLiteral("none") << QStringList();
    }

    void emblemForEachState()
    {
        QFETCH(QByteArray, state);
        QFETCH(QString, pin);
        QFETCH(QStringList, expected);
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), state));
        if (pin == QLatin1String("explicit")) {
            QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive/doc.bin"))));
        } else if (pin == QLatin1String("ancestor")) {
            QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive"))));
        }
        QCOMPARE(overlays(tree.path(QStringLiteral("OneDrive/doc.bin"))), expected);
    }

    void emblemWhileWaitingToUpload_data()
    {
        QTest::addColumn<QByteArray>("state"); // "folder" makes a folder
        QTest::addColumn<QByteArray>("upload");
        QTest::addColumn<QStringList>("expected");
        QTest::newRow("changed here, pending: syncing") << QByteArray("hydrated") << QByteArray("pending") << Syncing;
        QTest::newRow("changed here, uploading: syncing") << QByteArray("hydrated") << QByteArray("uploading") << Syncing;
        QTest::newRow("changed here, blocked: error") << QByteArray("hydrated") << QByteArray("blocked") << Error;
        QTest::newRow("new here, no state yet, pending: syncing") << QByteArray() << QByteArray("pending") << Syncing;
        QTest::newRow("a new folder, pending: syncing") << QByteArray("folder") << QByteArray("pending") << Syncing;
        QTest::newRow("an upload value we do not know: the state's emblem") << QByteArray("hydrated") << QByteArray("later") << CheckFilled;
    }

    /// `user.konedrive.sync` wins over the state and the pin (every item
    /// here is pinned) while it is there.
    void emblemWhileWaitingToUpload()
    {
        QFETCH(QByteArray, state);
        QFETCH(QByteArray, upload);
        QFETCH(QStringList, expected);
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        const QString item = tree.path(QStringLiteral("OneDrive/item"));
        QVERIFY(state == "folder" ? tree.dir(QStringLiteral("OneDrive/item")) : tree.file(QStringLiteral("OneDrive/item"), state));
        QVERIFY(testsupport::pin(item));
        QVERIFY(setUploadState(item, upload));
        QCOMPARE(overlays(item), expected);
    }

    /// A placeholder carried out of the sync folder keeps its attribute, and
    /// gets no emblem there; the same attribute inside the folder does.
    void noEmblemOutsideRoot()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.dir(QStringLiteral("Documents")));
        QVERIFY(tree.file(QStringLiteral("Documents/moved-out.bin"), "online-only"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/inside.bin"), "online-only"));
        QCOMPARE(overlays(tree.path(QStringLiteral("Documents/moved-out.bin"))), QStringList());
        QCOMPARE(overlays(tree.path(QStringLiteral("OneDrive/inside.bin"))), Cloud);
    }

    void rootFoundAboveTheFile_data()
    {
        QTest::addColumn<QString>("shownAs");
        QTest::newRow("directly in the root") << QStringLiteral("OneDrive/doc.bin");
        QTest::newRow("three folders down") << QStringLiteral("OneDrive/a/b/c/doc.bin");
        QTest::newRow("through a symbolic link to the root") << QStringLiteral("link/a/b/c/doc.bin");
    }

    void rootFoundAboveTheFile()
    {
        QFETCH(QString, shownAs);
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "hydrated"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/a/b/c/doc.bin"), "hydrated"));
        QVERIFY(tree.symlink(QStringLiteral("OneDrive"), QStringLiteral("link")));
        QCOMPARE(overlays(tree.path(shownAs)), CheckOutline);
    }

    void linkOutOfTheRootLeadsOutside_data()
    {
        QTest::addColumn<QString>("shownAs");
        QTest::newRow("through a link in the root to a folder outside") << QStringLiteral("OneDrive/escape/f.bin");
        QTest::newRow("through .. out of the root") << QStringLiteral("OneDrive/../Elsewhere/f.bin");
    }

    /// What counts is where a file physically is, not how its path is
    /// spelled: a symbolic link inside the sync folder that points out of it
    /// does not bring the folder it points to inside.
    void linkOutOfTheRootLeadsOutside()
    {
        QFETCH(QString, shownAs);
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("Elsewhere/f.bin"), "online-only"));
        QVERIFY(tree.symlink(QStringLiteral("Elsewhere"), QStringLiteral("OneDrive/escape")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/inside.bin"), "online-only"));
        QCOMPARE(overlays(tree.path(shownAs)), QStringList());
        QCOMPARE(overlays(tree.path(QStringLiteral("OneDrive/inside.bin"))), Cloud);
    }

    /// An unpinned folder gets no emblem, and a symbolic link never does --
    /// it does not borrow the state or the pin of the placeholder it points
    /// to.
    void noEmblemOnUnpinnedFolderOrSymlink()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.dir(QStringLiteral("OneDrive/folder")));
        QVERIFY(setState(tree.path(QStringLiteral("OneDrive/folder")), "online-only"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        QVERIFY(tree.symlink(QStringLiteral("OneDrive/doc.bin"), QStringLiteral("OneDrive/link.bin")));
        QCOMPARE(overlays(tree.path(QStringLiteral("OneDrive/folder"))), QStringList());
        QCOMPARE(overlays(tree.path(QStringLiteral("OneDrive/link.bin"))), QStringList());
        QCOMPARE(overlays(tree.path(QStringLiteral("OneDrive/doc.bin"))), Cloud);
    }

    /// A folder that is effectively pinned -- itself, or through an
    /// ancestor -- gets the filled check, same as a pinned hydrated file
    /// (review #5).
    void pinnedFolderGetsTheFilledCheck()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.dir(QStringLiteral("OneDrive/explicit")));
        QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive/explicit"))));
        QVERIFY(tree.dir(QStringLiteral("OneDrive/byAncestor/sub")));
        QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive/byAncestor"))));
        QCOMPARE(overlays(tree.path(QStringLiteral("OneDrive/explicit"))), CheckFilled);
        QCOMPARE(overlays(tree.path(QStringLiteral("OneDrive/byAncestor/sub"))), CheckFilled);
    }

    /// Emblems come from the file's attribute, not from the daemon: with no
    /// konedrived on the bus at all, they are still drawn.
    void emblemsNeedNoDaemon()
    {
        QVERIFY(!QDBusConnection::sessionBus().interface()->isServiceRegistered(QStringLiteral("org.konedrive.Daemon")));
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        QCOMPARE(overlays(tree.path(QStringLiteral("OneDrive/doc.bin"))), Cloud);
    }

    /// A download or a free-up changes the emblem without a reload: the
    /// daemon writes the state through a descriptor it holds, and the plugin
    /// announces the new overlays.
    void emblemFollowsStateChanges()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/sub/doc.bin"), "online-only"));
        const QString doc = tree.path(QStringLiteral("OneDrive/sub/doc.bin"));
        QCOMPARE(overlays(doc), Cloud);

        QSignalSpy changed(m_plugin, &KOverlayIconPlugin::overlaysChanged);
        QVERIFY(setState(doc, "hydrating"));
        QTRY_COMPARE(changed.count(), 1);
        QCOMPARE(changed.at(0).at(0).toUrl(), url(doc));
        QCOMPARE(changed.at(0).at(1).toStringList(), Syncing);

        // As the daemon does it: fsetxattr on its own descriptor.
        const int fd = ::open(native(doc).constData(), O_RDONLY | O_CLOEXEC);
        QVERIFY(fd >= 0);
        QCOMPARE(::fsetxattr(fd, "user.konedrive.state", "hydrated", 8, 0), 0);
        ::close(fd);
        QTRY_COMPARE(changed.count(), 2);
        QCOMPARE(changed.at(1).at(1).toStringList(), CheckOutline);

        QVERIFY(removeAttribute(doc, "user.konedrive.state"));
        QTRY_COMPARE(changed.count(), 3);
        QCOMPARE(changed.at(2).at(1).toStringList(), QStringList());
    }

    /// A folder Dolphin already asked about while it was ordinary, emptied
    /// and then registered as the sync folder: the files that appear in it
    /// get emblems, rather than the stale "not in a root".
    void folderRegisteredLaterGetsEmblems()
    {
        Tree tree;
        QVERIFY(tree.dir(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/old.txt")));
        QCOMPARE(overlays(tree.path(QStringLiteral("OneDrive/old.txt"))), QStringList());

        QVERIFY(QFile::remove(tree.path(QStringLiteral("OneDrive/old.txt"))));
        QVERIFY(markRoot(tree.path(QStringLiteral("OneDrive"))));
        QVERIFY(tree.file(QStringLiteral("OneDrive/new.bin"), "online-only"));
        QTRY_COMPARE(overlays(tree.path(QStringLiteral("OneDrive/new.bin"))), Cloud);
    }

    /// A folder that stops being a sync root takes its files' emblems with it.
    void rootLosingItsMarkClearsEmblems()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/sub/doc.bin"), "online-only"));
        const QString doc = tree.path(QStringLiteral("OneDrive/sub/doc.bin"));
        QCOMPARE(overlays(doc), Cloud);

        QSignalSpy changed(m_plugin, &KOverlayIconPlugin::overlaysChanged);
        QVERIFY(removeAttribute(tree.path(QStringLiteral("OneDrive")), "user.konedrive.root"));
        QTRY_COMPARE(changed.count(), 1);
        QCOMPARE(changed.at(0).at(0).toUrl(), url(doc));
        QCOMPARE(changed.at(0).at(1).toStringList(), QStringList());
        QCOMPARE(overlays(doc), QStringList());
    }
};

QTEST_MAIN(OverlayPluginTest)

#include "overlayplugintest.moc"
