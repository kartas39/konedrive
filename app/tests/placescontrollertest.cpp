#include "accountsmodel.h"
#include "daemoncontroller.h"
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
const QString OldKey = QStringLiteral("konedrive");
const QUrl Personal = QUrl::fromLocalFile(QStringLiteral("/home/u/OneDrive"));
const QUrl Family = QUrl::fromLocalFile(QStringLiteral("/home/u/Family"));
const QUrl Documents = QUrl::fromLocalFile(QStringLiteral("/home/u/Documents"));
const QUrl Music = QUrl::fromLocalFile(QStringLiteral("/home/u/Music"));
}

/// PlacesController keeps one Places entry per account folder, named
/// "OneDrive — <label>" and found again by a bookmark metadata tag (the
/// account's id) rather than by url. XDG_DATA_HOME and XDG_CONFIG_HOME point
/// at temporary directories, wiped clean before each test (the way
/// AutostartTest wipes XDG_CONFIG_HOME), so this never touches the user's own
/// ~/.local/share/user-places.xbel or konedriverc, and one test's
/// PlacesSettings choice or leftover entry cannot leak into the next.
class PlacesControllerTest : public QObject
{
    Q_OBJECT

private:
    QTemporaryDir m_data;
    QTemporaryDir m_config;
    std::unique_ptr<FakeDaemon> m_daemon;
    std::unique_ptr<DaemonController> m_manager;
    std::unique_ptr<AccountsModel> m_accounts;

    /// The fake daemon with these accounts; `folders` gives each its folder
    /// (an empty url: none). Started unless `start` is false.
    void startFake(const QStringList &labels, const QList<QUrl> &folders, bool start = true)
    {
        m_daemon = std::make_unique<FakeDaemon>(labels);
        for (int i = 0; i < folders.size(); ++i) {
            if (!folders.at(i).isEmpty()) {
                setFolder(m_daemon->object(i), folders.at(i));
            }
        }
        if (start) {
            QVERIFY(m_daemon->start());
        }
    }

    void follow()
    {
        m_manager = std::make_unique<DaemonController>();
        m_accounts = std::make_unique<AccountsModel>(m_manager.get());
    }

    static void setFolder(FakeAccountObject *object, const QUrl &folder)
    {
        object->sync->set({{QStringLiteral("RootPath"), folder.toLocalFile()},
                           {QStringLiteral("RootState"), QStringLiteral("ready")},
                           {QStringLiteral("RootSource"), QStringLiteral("onedrive")}});
    }

    static void forget(FakeAccountObject *object)
    {
        object->sync->set({{QStringLiteral("RootPath"), QString()}, {QStringLiteral("RootState"), QStringLiteral("none")}, {QStringLiteral("RootSource"), QString()}});
    }

    static QModelIndex entry(KFilePlacesModel *model, const QString &id)
    {
        for (int row = 0; row < model->rowCount(); ++row) {
            const QModelIndex idx = model->index(row, 0);
            if (model->bookmarkForIndex(idx).metaDataItem(PlacesController::AccountKey) == id) {
                return idx;
            }
        }
        return QModelIndex();
    }

    static int tagged(KFilePlacesModel *model)
    {
        int count = 0;
        for (int row = 0; row < model->rowCount(); ++row) {
            const KBookmark bookmark = model->bookmarkForIndex(model->index(row, 0));
            if (!bookmark.metaDataItem(PlacesController::AccountKey).isEmpty() || bookmark.metaDataItem(OldKey) == QLatin1String("1")) {
                ++count;
            }
        }
        return count;
    }

    static int rowOf(KFilePlacesModel *model, const QUrl &url)
    {
        for (int row = 0; row < model->rowCount(); ++row) {
            if (model->url(model->index(row, 0)) == url) {
                return row;
            }
        }
        return -1;
    }

    /// A place as a user (or an earlier version) left it, with an optional tag.
    static void seed(KFilePlacesModel &model, const QString &text, const QUrl &url, const QString &key = QString(), const QString &value = QString())
    {
        model.addPlace(text, url, QStringLiteral("folder"));
        if (key.isEmpty()) {
            return;
        }
        const QModelIndex idx = model.index(rowOf(&model, url), 0);
        KBookmark bookmark = model.bookmarkForIndex(idx);
        bookmark.setMetaDataItem(key, value);
        // editPlace with the same values saves nothing; refresh() saves.
        model.refresh();
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
        m_accounts.reset();
        m_manager.reset();
        if (m_daemon) {
            m_daemon->stop();
        }
        m_daemon.reset();
    }

    void addsAnEntryPerAccountFolder()
    {
        startFake({QStringLiteral("Personal"), QStringLiteral("Family")}, {Personal, QUrl()});
        follow();
        PlacesSettings settings;
        PlacesController controller(m_accounts.get(), &settings);
        KFilePlacesModel *model = controller.model();

        QTRY_VERIFY(entry(model, fake::idFor(1)).isValid());
        QCOMPARE(model->url(entry(model, fake::idFor(1))), Personal);
        QCOMPARE(model->text(entry(model, fake::idFor(1))), QStringLiteral("OneDrive — Personal"));
        QVERIFY(!entry(model, fake::idFor(2)).isValid());

        setFolder(m_daemon->object(1), Family);
        QTRY_VERIFY(entry(model, fake::idFor(2)).isValid());
        QCOMPARE(model->url(entry(model, fake::idFor(2))), Family);
        QCOMPARE(model->text(entry(model, fake::idFor(2))), QStringLiteral("OneDrive — Family"));
        QCOMPARE(tagged(model), 2);
    }

    void updatesTheUrlWhenTheFolderChanges()
    {
        startFake({QStringLiteral("Personal")}, {Personal});
        follow();
        PlacesSettings settings;
        PlacesController controller(m_accounts.get(), &settings);
        KFilePlacesModel *model = controller.model();
        QTRY_VERIFY(entry(model, fake::idFor(1)).isValid());

        setFolder(m_daemon->object(0), Family);
        QTRY_COMPARE(model->url(entry(model, fake::idFor(1))), Family);
        // Still the same, single, tagged entry: no duplicate was left behind.
        QCOMPARE(tagged(model), 1);
    }

    void renamingTheAccountRenamesItsEntry()
    {
        startFake({QStringLiteral("Personal")}, {Personal});
        follow();
        PlacesSettings settings;
        PlacesController controller(m_accounts.get(), &settings);
        KFilePlacesModel *model = controller.model();
        QTRY_VERIFY(entry(model, fake::idFor(1)).isValid());
        const int row = entry(model, fake::idFor(1)).row();

        m_daemon->account->set({{QStringLiteral("Label"), QStringLiteral("Home")}});
        QTRY_COMPARE(model->text(entry(model, fake::idFor(1))), QStringLiteral("OneDrive — Home"));
        QCOMPARE(entry(model, fake::idFor(1)).row(), row);
        QCOMPARE(tagged(model), 1);
    }

    void forgettingOrRemovingRemovesTheEntry()
    {
        startFake({QStringLiteral("Personal"), QStringLiteral("Family")}, {Personal, Family});
        follow();
        PlacesSettings settings;
        PlacesController controller(m_accounts.get(), &settings);
        KFilePlacesModel *model = controller.model();
        QTRY_COMPARE(tagged(model), 2);

        forget(m_daemon->object(0));
        QTRY_VERIFY(!entry(model, fake::idFor(1)).isValid());
        QVERIFY(entry(model, fake::idFor(2)).isValid());

        m_daemon->removeAccount(m_daemon->object(1)->path);
        QTRY_VERIFY(!entry(model, fake::idFor(2)).isValid());
        QCOMPARE(tagged(model), 0);
    }

    void turningTheSwitchOffRemovesTheEntries()
    {
        startFake({QStringLiteral("Personal"), QStringLiteral("Family")}, {Personal, Family});
        follow();
        PlacesSettings settings;
        PlacesController controller(m_accounts.get(), &settings);
        QTRY_COMPARE(tagged(controller.model()), 2);

        settings.setEnabled(false);
        QCOMPARE(tagged(controller.model()), 0);
        settings.setEnabled(true);
        QCOMPARE(tagged(controller.model()), 2);
    }

    /// Switching "Show in Places" off needs no account's answer: the entries
    /// go even while the daemon is not running (the user's stay).
    void switchingOffWorksWithoutTheDaemon()
    {
        {
            KFilePlacesModel seedModel;
            seed(seedModel, QStringLiteral("OneDrive — Personal"), Personal, PlacesController::AccountKey, fake::idFor(1));
            seed(seedModel, QStringLiteral("OneDrive"), Family, OldKey, QStringLiteral("1"));
            seed(seedModel, QStringLiteral("Music"), Music);
        }
        startFake({QStringLiteral("Personal")}, {Personal}, false);
        follow();
        PlacesSettings settings;
        PlacesController controller(m_accounts.get(), &settings);
        QCOMPARE(tagged(controller.model()), 2);

        settings.setEnabled(false);
        QCOMPARE(tagged(controller.model()), 0);
        QVERIFY(rowOf(controller.model(), Music) >= 0);
    }

    void aUsersOwnEntryIsLeftAlone()
    {
        // A place the user made, before konedrive ever runs: same kind of
        // url, no metadata tag.
        {
            KFilePlacesModel seedModel;
            seed(seedModel, QStringLiteral("My Stuff"), Documents);
        }

        startFake({QStringLiteral("Personal")}, {Personal});
        follow();
        PlacesSettings settings;
        PlacesController controller(m_accounts.get(), &settings);
        KFilePlacesModel *model = controller.model();
        QTRY_VERIFY(entry(model, fake::idFor(1)).isValid());

        const QModelIndex mine = model->index(rowOf(model, Documents), 0);
        QVERIFY(mine.isValid());
        QCOMPARE(model->text(mine), QStringLiteral("My Stuff"));
        QCOMPARE(model->bookmarkForIndex(mine).metaDataItem(PlacesController::AccountKey), QString());

        // Forgetting the folder removes only konedrive's own entry.
        forget(m_daemon->object(0));
        QTRY_VERIFY(!entry(model, fake::idFor(1)).isValid());
        QVERIFY(rowOf(model, Documents) >= 0);
    }

    /// The single-account versions' entry (`konedrive` = `1`) at an account's
    /// folder becomes that account's, renamed, where it was in the panel; one
    /// at any other url goes.
    void theOldEntryIsTakenOverInPlace()
    {
        {
            KFilePlacesModel seedModel;
            seed(seedModel, QStringLiteral("My Stuff"), Documents);
            seed(seedModel, QStringLiteral("OneDrive"), Personal, OldKey, QStringLiteral("1"));
            seed(seedModel, QStringLiteral("Music"), Music);
            seed(seedModel, QStringLiteral("OneDrive"), QUrl::fromLocalFile(QStringLiteral("/home/u/Stale")), OldKey, QStringLiteral("1"));
        }

        startFake({QStringLiteral("Personal")}, {Personal});
        follow();
        PlacesSettings settings;
        PlacesController controller(m_accounts.get(), &settings);
        KFilePlacesModel *model = controller.model();

        QTRY_VERIFY(entry(model, fake::idFor(1)).isValid());
        const QModelIndex taken = entry(model, fake::idFor(1));
        QCOMPARE(model->url(taken), Personal);
        QCOMPARE(model->text(taken), QStringLiteral("OneDrive — Personal"));
        QCOMPARE(model->bookmarkForIndex(taken).metaDataItem(OldKey), QString());
        // Still between the user's two places: the same entry, not a new one at the end.
        QCOMPARE(taken.row(), rowOf(model, Documents) + 1);
        QCOMPARE(rowOf(model, Music), taken.row() + 1);
        QCOMPARE(rowOf(model, QUrl::fromLocalFile(QStringLiteral("/home/u/Stale"))), -1);
        QCOMPARE(tagged(model), 1);
    }

    /// Until the daemon and every account have answered, an entry that looks
    /// unwanted may only be unknown yet: it stays, and keeps its place.
    void nothingIsTouchedUntilEveryAccountHasAnswered()
    {
        {
            KFilePlacesModel seedModel;
            seed(seedModel, QStringLiteral("OneDrive — Personal"), Personal, PlacesController::AccountKey, fake::idFor(1));
            seed(seedModel, QStringLiteral("Music"), Music);
        }

        startFake({QStringLiteral("Personal")}, {Personal}, false);
        follow();
        PlacesSettings settings;
        PlacesController controller(m_accounts.get(), &settings);
        KFilePlacesModel *model = controller.model();
        QTest::qWait(300);
        QVERIFY(entry(model, fake::idFor(1)).isValid());
        const int row = entry(model, fake::idFor(1)).row();
        QCOMPARE(rowOf(model, Music), row + 1);

        QVERIFY(m_daemon->start());
        QTRY_VERIFY(m_accounts->count() == 1 && m_accounts->at(0)->sync()->serviceAvailable() && m_accounts->at(0)->account()->serviceAvailable());
        QCoreApplication::processEvents();
        QCOMPARE(entry(model, fake::idFor(1)).row(), row);
        QCOMPARE(tagged(model), 1);
    }
};

QTEST_MAIN(PlacesControllerTest)

#include "placescontrollertest.moc"
