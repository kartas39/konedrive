// The overlay logic on its own: the per-directory root cache, what keeps it
// true, the bound on what it watches, and the icon names it hands Dolphin.

#include "overlayengine.h"
#include "testsupport.h"

#include <QDir>
#include <QIcon>
#include <QSignalSpy>
#include <QStandardPaths>
#include <QTest>

#include <cstdlib>
#include <cstring>

#include <unistd.h>

using namespace konedrive;
using namespace testsupport;

namespace
{
const QStringList Cloud{QStringLiteral("cloudstatus")};
const QStringList Syncing{QStringLiteral("state-sync")};
const QStringList CheckOutline{QStringLiteral("dialog-ok")};
const QStringList CheckFilled{QStringLiteral("emblem-checked")};
const QStringList Error{QStringLiteral("state-error")};

/// inotify watches this process holds, as the kernel counts them
/// (/proc/self/fdinfo lists one "inotify wd:" line per watch).
int kernelWatchCount()
{
    int count = 0;
    const QStringList fds = QDir(QStringLiteral("/proc/self/fd")).entryList(QDir::NoDotAndDotDot | QDir::AllEntries | QDir::System);
    for (const QString &fd : fds) {
        char target[64] = {};
        const QByteArray link = native(QStringLiteral("/proc/self/fd/") + fd);
        if (::readlink(link.constData(), target, sizeof target - 1) < 0 || std::strcmp(target, "anon_inode:inotify") != 0) {
            continue;
        }
        QFile info(QStringLiteral("/proc/self/fdinfo/") + fd);
        if (!info.open(QIODevice::ReadOnly)) {
            continue;
        }
        const QList<QByteArray> lines = info.readAll().split('\n');
        for (const QByteArray &line : lines) {
            if (line.startsWith("inotify wd:")) {
                ++count;
            }
        }
    }
    return count;
}

int maxQueuedEvents()
{
    QFile limit(QStringLiteral("/proc/sys/fs/inotify/max_queued_events"));
    if (!limit.open(QIODevice::ReadOnly)) {
        return -1;
    }
    return limit.readAll().trimmed().toInt();
}

QStringList changedOverlaysFor(const QSignalSpy &spy, const QUrl &url)
{
    for (const QList<QVariant> &arguments : spy) {
        if (arguments.at(0).toUrl() == url) {
            return arguments.at(1).toStringList();
        }
    }
    return {QStringLiteral("(no change announced)")};
}
} // namespace

class OverlayEngineTest : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void initTestCase()
    {
        QStandardPaths::setTestModeEnabled(true);
    }

    void rootAnswerIsCachedPerDirectory_data()
    {
        QTest::addColumn<bool>("inRoot");
        QTest::newRow("inside a root") << true;
        QTest::newRow("outside any root") << false;
    }

    /// The first file of a directory looks for the root mark up the tree;
    /// the other files of that directory do not look again.
    void rootAnswerIsCachedPerDirectory()
    {
        QFETCH(bool, inRoot);
        Tree tree;
        QVERIFY(inRoot ? tree.root(QStringLiteral("top")) : tree.dir(QStringLiteral("top")));
        for (int i = 0; i < 50; ++i) {
            QVERIFY(tree.file(QStringLiteral("top/a/b/%1.bin").arg(i), "online-only"));
        }

        int reads = 0;
        OverlayEngine engine(nullptr, OverlayEngine::DefaultDirectoryLimit, [&reads](const QString &dir) {
            ++reads;
            return hasRootMark(dir);
        });

        const QStringList expected = inRoot ? Cloud : QStringList();
        QCOMPARE(engine.overlays(url(tree.path(QStringLiteral("top/a/b/0.bin")))), expected);
        const int afterFirst = reads;
        QVERIFY(afterFirst >= 3); // b, a and top at least
        for (int i = 1; i < 50; ++i) {
            QCOMPARE(engine.overlays(url(tree.path(QStringLiteral("top/a/b/%1.bin").arg(i)))), expected);
        }
        QCOMPARE(reads, afterFirst);
    }

    /// However many folders Dolphin shows, the watches held stay at the
    /// limit -- counted by the kernel, not by the engine -- and the folder
    /// asked about last still updates live.
    void watchSetIsBounded()
    {
        const int limit = OverlayEngine::DefaultDirectoryLimit;
        const int folders = limit + 50;
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        for (int i = 0; i < folders; ++i) {
            QVERIFY(tree.file(QStringLiteral("OneDrive/d%1/f.bin").arg(i), "online-only"));
        }

        const int before = kernelWatchCount();
        OverlayEngine engine;
        for (int i = 0; i < folders; ++i) {
            QCOMPARE(engine.overlays(url(tree.path(QStringLiteral("OneDrive/d%1/f.bin").arg(i)))), Cloud);
            QVERIFY2(kernelWatchCount() - before <= limit, qPrintable(QStringLiteral("%1 watches after %2 folders").arg(kernelWatchCount() - before).arg(i + 1)));
        }
        QCOMPARE(kernelWatchCount() - before, limit);
        QCOMPARE(engine.watchCount(), limit);
        QCOMPARE(engine.directoryCount(), limit);

        QSignalSpy changed(&engine, &OverlayEngine::overlaysChanged);
        const QString last = tree.path(QStringLiteral("OneDrive/d%1/f.bin").arg(folders - 1));
        QVERIFY(setState(last, "hydrated"));
        QTRY_COMPARE(changedOverlaysFor(changed, url(last)), CheckOutline);
    }

    /// A folder whose watch was evicted keeps no answer: asked about again,
    /// it is worked out afresh, so a root mark set while nothing watched it
    /// is seen.
    void evictedFolderIsWorkedOutAgain()
    {
        Tree tree;
        QVERIFY(tree.dir(QStringLiteral("Later")));
        QVERIFY(tree.file(QStringLiteral("Later/old.bin")));
        OverlayEngine engine(nullptr, 4);
        QCOMPARE(engine.overlays(url(tree.path(QStringLiteral("Later/old.bin")))), QStringList());

        for (int i = 0; i < 4; ++i) {
            QVERIFY(tree.file(QStringLiteral("other%1/f.bin").arg(i)));
            engine.overlays(url(tree.path(QStringLiteral("other%1/f.bin").arg(i))));
        }
        // Later's watch is gone now, so marking it makes no event at all.
        QVERIFY(markRoot(tree.path(QStringLiteral("Later"))));
        QVERIFY(tree.file(QStringLiteral("Later/new.bin"), "online-only"));
        QCOMPARE(engine.overlays(url(tree.path(QStringLiteral("Later/new.bin")))), Cloud);
    }

    /// A sync folder moved away is no longer a root at its old path: a new
    /// ordinary folder there does not inherit it.
    void movedRootStopsCountingAtItsOldPath()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        OverlayEngine engine;
        QCOMPARE(engine.overlays(url(tree.path(QStringLiteral("OneDrive/doc.bin")))), Cloud);

        QVERIFY(QDir().rename(tree.path(QStringLiteral("OneDrive")), tree.path(QStringLiteral("OneDrive-old"))));
        QVERIFY(tree.dir(QStringLiteral("OneDrive")));
        // A placeholder that left the sync folder keeps its attribute.
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        QTRY_COMPARE(engine.overlays(url(tree.path(QStringLiteral("OneDrive/doc.bin")))), QStringList());
    }

    /// One folder reached through two paths (one of them a symbolic link)
    /// shares one inotify watch; a change is announced under both paths.
    void oneFolderThroughTwoPaths()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        QVERIFY(tree.symlink(QStringLiteral("OneDrive"), QStringLiteral("link")));
        OverlayEngine engine;
        const QUrl direct = url(tree.path(QStringLiteral("OneDrive/doc.bin")));
        const QUrl linked = url(tree.path(QStringLiteral("link/doc.bin")));
        QCOMPARE(engine.overlays(direct), Cloud);
        QCOMPARE(engine.overlays(linked), Cloud);

        QSignalSpy changed(&engine, &OverlayEngine::overlaysChanged);
        QVERIFY(setState(tree.path(QStringLiteral("OneDrive/doc.bin")), "hydrated"));
        QTRY_COMPARE(changed.count(), 2);
        QCOMPARE(changedOverlaysFor(changed, direct), CheckOutline);
        QCOMPARE(changedOverlaysFor(changed, linked), CheckOutline);
    }

    /// When the kernel's event queue overflows it drops events and says so;
    /// a file whose change was among those dropped is still updated.
    void changeLostToQueueOverflowIsStillShown()
    {
        const int queueLimit = maxQueuedEvents();
        QVERIFY(queueLimit > 0);
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/a.bin"), "online-only"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/b.bin"), "online-only"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        const QString a = tree.path(QStringLiteral("OneDrive/a.bin"));
        const QString b = tree.path(QStringLiteral("OneDrive/b.bin"));
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));

        OverlayEngine engine;
        QCOMPARE(engine.overlays(url(a)), Cloud);
        QCOMPARE(engine.overlays(url(b)), Cloud);
        QCOMPARE(engine.overlays(url(doc)), Cloud);
        QSignalSpy changed(&engine, &OverlayEngine::overlaysChanged);

        // Without returning to the event loop: more changes than the queue
        // holds. Alternating files, so the kernel cannot merge them.
        for (int i = 0; i <= queueLimit; ++i) {
            QVERIFY(setState(i % 2 ? a : b, i % 4 < 2 ? "online-only" : "hydrating"));
        }
        QVERIFY(setState(doc, "hydrated"));
        QTRY_COMPARE(changedOverlaysFor(changed, url(doc)), CheckOutline);
    }

    void physicalDirectoryMatchesTheKernel_data()
    {
        QTest::addColumn<QString>("spelled");
        QTest::newRow("a plain folder") << QStringLiteral("OneDrive/a");
        QTest::newRow("an absolute link") << QStringLiteral("absolute/a");
        QTest::newRow("a relative link") << QStringLiteral("OneDrive/relative");
        QTest::newRow("a chain of links") << QStringLiteral("chain");
        QTest::newRow(".. after a link is the target's parent") << QStringLiteral("OneDrive/relative/..");
        QTest::newRow(". and .. spelled out") << QStringLiteral("OneDrive/./a/../a");
        QTest::newRow("a link loop") << QStringLiteral("loop/a");
        QTest::newRow("a missing folder") << QStringLiteral("OneDrive/missing/a");
    }

    /// The root walk goes over the physical path, resolved without opening
    /// anything; it has to name the same folder realpath(3) does, and fail
    /// where it fails.
    void physicalDirectoryMatchesTheKernel()
    {
        QFETCH(QString, spelled);
        Tree tree;
        QVERIFY(tree.dir(QStringLiteral("OneDrive/a")));
        QVERIFY(tree.dir(QStringLiteral("Elsewhere/b")));
        QVERIFY(tree.symlink(QStringLiteral("OneDrive"), QStringLiteral("absolute")));
        QVERIFY(::symlink("../Elsewhere/b", native(tree.path(QStringLiteral("OneDrive/relative"))).constData()) == 0);
        QVERIFY(::symlink("absolute", native(tree.path(QStringLiteral("chain"))).constData()) == 0);
        QVERIFY(::symlink("loop", native(tree.path(QStringLiteral("loop"))).constData()) == 0);

        const QString path = tree.path(spelled);
        char *kernel = ::realpath(native(path).constData(), nullptr);
        const QString expected = kernel ? QFile::decodeName(kernel) : QString();
        ::free(kernel);
        QCOMPARE(physicalDirectory(path), expected);
    }

    /// The names handed to Dolphin -- the four emblems and the two menu
    /// icons -- are real icons of the installed Breeze and Breeze Dark
    /// themes, not guesses.
    void iconNamesExistInBreeze_data()
    {
        QTest::addColumn<QString>("theme");
        QTest::newRow("breeze") << QStringLiteral("breeze");
        QTest::newRow("breeze-dark") << QStringLiteral("breeze-dark");
    }

    void iconNamesExistInBreeze()
    {
        QFETCH(QString, theme);
        QStringList names = overlayNames(Emblem::Cloud) + overlayNames(Emblem::Syncing) + overlayNames(Emblem::CheckOutline) + overlayNames(Emblem::CheckFilled)
            + overlayNames(Emblem::Error);
        names << QString::fromLatin1(AlwaysKeepIcon) << QString::fromLatin1(FreeUpSpaceIcon);
        QCOMPARE(names.size(), 7);
        // The installed themes (XDG_DATA_DIRS/icons), not the copy of Breeze
        // some KDE libraries compile in under :/icons.
        QIcon::setThemeSearchPaths(QStandardPaths::locateAll(QStandardPaths::GenericDataLocation, QStringLiteral("icons"), QStandardPaths::LocateDirectory));
        QIcon::setThemeName(theme);
        QIcon::setFallbackThemeName(QString());
        for (const QString &name : std::as_const(names)) {
            QVERIFY2(QIcon::hasThemeIcon(name), qPrintable(name + QStringLiteral(" is not in ") + theme));
        }
    }

    /// The outline check becomes the filled one, without asking again, when
    /// the file itself is pinned; and a pin set on a directory Dolphin has
    /// browsed (so it is cached here) reaches every hydrated file below it,
    /// not just the ones directly in it.
    void pinChangesTheCheckEmblemLiveOnTheFileAndOnAnAncestor()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/a.bin"), "hydrated"));
        QVERIFY(tree.file(QStringLiteral("OneDrive/sub/b.bin"), "hydrated"));
        const QString a = tree.path(QStringLiteral("OneDrive/a.bin"));
        const QString b = tree.path(QStringLiteral("OneDrive/sub/b.bin"));
        OverlayEngine engine;
        QCOMPARE(engine.overlays(url(a)), CheckOutline);
        QCOMPARE(engine.overlays(url(b)), CheckOutline);

        QSignalSpy changed(&engine, &OverlayEngine::overlaysChanged);
        QVERIFY(testsupport::pin(a));
        QTRY_COMPARE(changedOverlaysFor(changed, url(a)), CheckFilled);

        // "sub" is watched too (Dolphin asked about b.bin, which is inside
        // it), so pinning it is announced without engine.overlays(b) being
        // called again.
        QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive/sub"))));
        QTRY_COMPARE(changedOverlaysFor(changed, url(b)), CheckFilled);
    }

    /// The upload state the daemon writes, and its removal at the commit,
    /// are followed live, as a state change is.
    void uploadStateIsFollowedLive()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "hydrated"));
        const QString doc = tree.path(QStringLiteral("OneDrive/doc.bin"));
        OverlayEngine engine;
        QCOMPARE(engine.overlays(url(doc)), CheckOutline);

        QSignalSpy changed(&engine, &OverlayEngine::overlaysChanged);
        QVERIFY(setUploadState(doc, "pending"));
        QTRY_COMPARE(changedOverlaysFor(changed, url(doc)), Syncing);
        changed.clear();
        QVERIFY(setUploadState(doc, "blocked"));
        QTRY_COMPARE(changedOverlaysFor(changed, url(doc)), Error);
        changed.clear();
        QVERIFY(removeAttribute(doc, "user.konedrive.sync"));
        QTRY_COMPARE(changedOverlaysFor(changed, url(doc)), CheckOutline);
    }

    /// An online-only file the sweep has queued because it is pinned draws
    /// the syncing emblem, the same one a file mid-hydration draws.
    void pinnedOnlineOnlyFileIsSyncing()
    {
        Tree tree;
        QVERIFY(tree.root(QStringLiteral("OneDrive")));
        QVERIFY(tree.file(QStringLiteral("OneDrive/doc.bin"), "online-only"));
        QVERIFY(testsupport::pin(tree.path(QStringLiteral("OneDrive/doc.bin"))));
        OverlayEngine engine;
        QCOMPARE(engine.overlays(url(tree.path(QStringLiteral("OneDrive/doc.bin")))), Syncing);
    }
};

QTEST_MAIN(OverlayEngineTest)

#include "overlayenginetest.moc"
