#include "fakedaemon.h"
#include "qmlregistration.h"

#include <KLocalizedQmlContext>
#include <KLocalizedString>

#include <QApplication>
#include <QCryptographicHash>
#include <QDir>
#include <QDirIterator>
#include <QFile>
#include <QFileInfo>
#include <QQmlApplicationEngine>
#include <QQuickStyle>
#include <QQuickWindow>
#include <QStandardPaths>
#include <QTemporaryDir>
#include <QTest>

/// The window's QML runs from the binary, never from Qt's disk cache: with
/// the cache on and empty, loading the window writes no unit for its files
/// (issue #88). Qt trusts a cached unit of a resource file for as long as
/// the executable's modification time is unchanged, so a unit from there
/// could be an earlier build's.
class QmlCacheTest : public QObject
{
    Q_OBJECT

private:
    /// Where Qt writes the unit of the resource file `path` (":/…"): the
    /// SHA-1 of the path, in the cache location's qmlcache directory.
    static QString unitFor(const QString &path)
    {
        const QByteArray hash = QCryptographicHash::hash(path.toUtf8(), QCryptographicHash::Sha1).toHex();
        return QStandardPaths::writableLocation(QStandardPaths::CacheLocation) + QStringLiteral("/qmlcache/")
            + QString::fromLatin1(hash) + QStringLiteral(".qmlc");
    }

    static QStringList cached()
    {
        const QString dir = QStandardPaths::writableLocation(QStandardPaths::CacheLocation) + QStringLiteral("/qmlcache");
        return QDir(dir).entryList(QDir::Files);
    }

private Q_SLOTS:
    void initTestCase()
    {
        // main() pointed XDG_CACHE_HOME at an empty directory and turned the cache on.
        QVERIFY(qEnvironmentVariableIsEmpty("QML_DISABLE_DISK_CACHE"));
        QVERIFY(cached().isEmpty());
        KLocalizedString::setApplicationDomain("konedrive");
        QQuickStyle::setStyle(QStringLiteral("org.kde.desktop"));
    }

    void theWindowLeavesNoUnitInTheDiskCache()
    {
        FakeDaemon fake({QStringLiteral("Personal")});
        fake.object(0)->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        fake.object(0)->sync->folder->set({{QStringLiteral("Path"), QStringLiteral("/home/u/Personal")},
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
        registerKonedriveQml(&daemon, &accounts, &current, &autostart, &progress, &places, &trayIcons);

        QQmlApplicationEngine engine;
        KLocalization::setupLocalizedContext(&engine);

        // The cache is on, and a unit lands where unitFor() says.
        engine.load(QUrl(QStringLiteral("qrc:/tests/CacheProbe.qml")));
        QVERIFY(!engine.rootObjects().isEmpty());
        QTRY_VERIFY2(QFileInfo::exists(unitFor(QStringLiteral(":/tests/CacheProbe.qml"))), qPrintable(cached().join(u' ')));

        engine.loadFromModule("org.konedrive.app.window", "Main");
        QCOMPARE(engine.rootObjects().size(), 2);
        auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constLast());
        QVERIFY(window);
        window->show();
        QTRY_COMPARE(accounts.count(), 1);
        QTRY_VERIFY(accounts.at(0)->sync()->serviceAvailable());
        const QStringList pages{QStringLiteral("status"), QStringLiteral("activity"), QStringLiteral("conflicts"),
                                QStringLiteral("skipped"), QStringLiteral("notUploaded"), QStringLiteral("account"),
                                QStringLiteral("settings")};
        for (const QString &page : pages) {
            QMetaObject::invokeMethod(window, "showPage", Q_ARG(QVariant, page));
        }
        QTest::qWait(200);

        // Every file of the module, so a page added later is checked too.
        QStringList files;
        QDirIterator it(QStringLiteral(":/qt/qml/org/konedrive/app/window"), {QStringLiteral("*.qml")}, QDir::Files,
                        QDirIterator::Subdirectories);
        while (it.hasNext()) {
            files << it.next();
        }
        QVERIFY(!files.isEmpty());
        for (const QString &file : std::as_const(files)) {
            QVERIFY2(!QFileInfo::exists(unitFor(file)), qPrintable(file + QStringLiteral(" was compiled at run time")));
        }

        fake.stop();
    }
};

int main(int argc, char *argv[])
{
    // Before the engine reads them: the disk cache on, in an empty cache of this run's own.
    QTemporaryDir cache;
    if (!cache.isValid()) {
        return 1;
    }
    qputenv("XDG_CACHE_HOME", QFile::encodeName(cache.path()));
    qunsetenv("QML_DISABLE_DISK_CACHE");
    qunsetenv("QML_DISK_CACHE_PATH");
    qunsetenv("QML_DISK_CACHE");
    QApplication app(argc, argv);
    QmlCacheTest test;
    return QTest::qExec(&test, argc, argv);
}

#include "qmlcachetest.moc"
