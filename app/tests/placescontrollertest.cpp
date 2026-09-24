#include "fakedaemon.h"
#include "placescontroller.h"
#include "placessettings.h"
#include "synccontroller.h"

#include <KBookmark>
#include <KFilePlacesModel>

#include <QDir>
#include <QFile>
#include <QStandardPaths>
#include <QTemporaryDir>
#include <QTest>
#include <QUrl>

#include <memory>

namespace
{
const QString MetaDataKey = QStringLiteral("konedrive");
}

/// PlacesController keeps one Places entry for the registered folder, found
/// again by a bookmark metadata tag rather than by url. XDG_DATA_HOME and
/// XDG_CONFIG_HOME point at temporary directories, wiped clean before each
/// test (the way AutostartTest wipes XDG_CONFIG_HOME), so this never touches
/// the user's own ~/.local/share/user-places.xbel or konedriverc, and one
/// test's PlacesSettings choice or leftover entry cannot leak into the next.
class PlacesControllerTest : public QObject
{
    Q_OBJECT

private:
    QTemporaryDir m_data;
    QTemporaryDir m_config;
    std::unique_ptr<FakeDaemon> m_daemon;
    FakeSync1 *m_fake = nullptr;

    void startFake()
    {
        m_daemon = std::make_unique<FakeDaemon>();
        m_fake = m_daemon->sync;
        QVERIFY(m_daemon->start());
    }

    static QModelIndex ourEntry(KFilePlacesModel *model)
    {
        for (int row = 0; row < model->rowCount(); ++row) {
            const QModelIndex idx = model->index(row, 0);
            if (model->bookmarkForIndex(idx).metaDataItem(MetaDataKey) == QLatin1String("1")) {
                return idx;
            }
        }
        return QModelIndex();
    }

private Q_SLOTS:
    void init()
    {
        QVERIFY(m_data.isValid());
        QVERIFY(m_config.isValid());
        QVERIFY(QDir(m_data.path()).removeRecursively());
        QVERIFY(QDir().mkpath(m_data.path()));
        QVERIFY(QDir(m_config.path()).removeRecursively());
        QVERIFY(QDir().mkpath(m_config.path()));
        qputenv("XDG_DATA_HOME", QFile::encodeName(m_data.path()));
        qputenv("XDG_CONFIG_HOME", QFile::encodeName(m_config.path()));
        QCOMPARE(QStandardPaths::writableLocation(QStandardPaths::GenericDataLocation), m_data.path());
        QCOMPARE(QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation), m_config.path());
    }

    void cleanup()
    {
        if (m_daemon) {
            m_daemon->stop();
        }
        m_daemon.reset();
        m_fake = nullptr;
    }

    void addsTheEntryForARegisteredFolder()
    {
        startFake();
        SyncController sync;
        QTRY_VERIFY(sync.serviceAvailable());
        PlacesSettings settings;
        PlacesController controller(&sync, &settings);
        QVERIFY(!ourEntry(controller.model()).isValid());

        m_fake->set({{QStringLiteral("RootPath"), QStringLiteral("/home/u/OneDrive")},
                     {QStringLiteral("RootState"), QStringLiteral("listing")},
                     {QStringLiteral("RootSource"), QStringLiteral("onedrive")}});
        QTRY_VERIFY(ourEntry(controller.model()).isValid());
        const QModelIndex idx = ourEntry(controller.model());
        QCOMPARE(controller.model()->url(idx), QUrl::fromLocalFile(QStringLiteral("/home/u/OneDrive")));
        QCOMPARE(controller.model()->text(idx), QStringLiteral("OneDrive"));
    }

    void updatesTheUrlWhenTheFolderChanges()
    {
        startFake();
        SyncController sync;
        QTRY_VERIFY(sync.serviceAvailable());
        PlacesSettings settings;
        PlacesController controller(&sync, &settings);

        m_fake->set({{QStringLiteral("RootPath"), QStringLiteral("/home/u/OneDrive")}, {QStringLiteral("RootState"), QStringLiteral("listing")}});
        QTRY_VERIFY(ourEntry(controller.model()).isValid());

        m_fake->set({{QStringLiteral("RootPath"), QStringLiteral("/home/u/OneDrive2")}});
        QTRY_COMPARE(controller.model()->url(ourEntry(controller.model())), QUrl::fromLocalFile(QStringLiteral("/home/u/OneDrive2")));
        // Still the same, single, tagged entry: no duplicate was left behind.
        int tagged = 0;
        for (int row = 0; row < controller.model()->rowCount(); ++row) {
            if (controller.model()->bookmarkForIndex(controller.model()->index(row, 0)).metaDataItem(MetaDataKey) == QLatin1String("1")) {
                ++tagged;
            }
        }
        QCOMPARE(tagged, 1);
    }

    void removesTheEntryWhenTheFolderIsForgotten()
    {
        startFake();
        SyncController sync;
        QTRY_VERIFY(sync.serviceAvailable());
        PlacesSettings settings;
        PlacesController controller(&sync, &settings);

        m_fake->set({{QStringLiteral("RootPath"), QStringLiteral("/home/u/OneDrive")}, {QStringLiteral("RootState"), QStringLiteral("listing")}});
        QTRY_VERIFY(ourEntry(controller.model()).isValid());

        m_fake->set({{QStringLiteral("RootPath"), QString()}, {QStringLiteral("RootState"), QStringLiteral("none")}, {QStringLiteral("RootSource"), QString()}});
        QTRY_VERIFY(!ourEntry(controller.model()).isValid());
    }

    void turningTheSwitchOffRemovesTheEntry()
    {
        startFake();
        SyncController sync;
        QTRY_VERIFY(sync.serviceAvailable());
        PlacesSettings settings;
        PlacesController controller(&sync, &settings);

        m_fake->set({{QStringLiteral("RootPath"), QStringLiteral("/home/u/OneDrive")}, {QStringLiteral("RootState"), QStringLiteral("listing")}});
        QTRY_VERIFY(ourEntry(controller.model()).isValid());

        settings.setEnabled(false);
        QVERIFY(!ourEntry(controller.model()).isValid());
    }

    void aUsersOwnEntryIsLeftAlone()
    {
        // A place the user made, before konedrive ever runs: same kind of
        // url, no metadata tag.
        {
            KFilePlacesModel seed;
            seed.addPlace(QStringLiteral("My Stuff"), QUrl::fromLocalFile(QStringLiteral("/home/u/Documents")), QStringLiteral("folder-documents"));
        }

        startFake();
        SyncController sync;
        QTRY_VERIFY(sync.serviceAvailable());
        PlacesSettings settings;
        PlacesController controller(&sync, &settings);

        m_fake->set({{QStringLiteral("RootPath"), QStringLiteral("/home/u/OneDrive")}, {QStringLiteral("RootState"), QStringLiteral("listing")}});
        QTRY_VERIFY(ourEntry(controller.model()).isValid());

        bool foundUserPlace = false;
        for (int row = 0; row < controller.model()->rowCount(); ++row) {
            const QModelIndex idx = controller.model()->index(row, 0);
            if (controller.model()->url(idx) == QUrl::fromLocalFile(QStringLiteral("/home/u/Documents"))) {
                foundUserPlace = true;
                QCOMPARE(controller.model()->text(idx), QStringLiteral("My Stuff"));
                QCOMPARE(controller.model()->bookmarkForIndex(idx).metaDataItem(MetaDataKey), QString());
            }
        }
        QVERIFY(foundUserPlace);

        // Forgetting the folder removes only konedrive's own entry.
        m_fake->set({{QStringLiteral("RootPath"), QString()}, {QStringLiteral("RootState"), QStringLiteral("none")}, {QStringLiteral("RootSource"), QString()}});
        QTRY_VERIFY(!ourEntry(controller.model()).isValid());
        bool stillThere = false;
        for (int row = 0; row < controller.model()->rowCount(); ++row) {
            if (controller.model()->url(controller.model()->index(row, 0)) == QUrl::fromLocalFile(QStringLiteral("/home/u/Documents"))) {
                stillThere = true;
            }
        }
        QVERIFY(stillThere);
    }
};

QTEST_MAIN(PlacesControllerTest)

#include "placescontrollertest.moc"
