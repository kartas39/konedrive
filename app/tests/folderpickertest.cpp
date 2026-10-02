#include "fakedaemon.h"
#include "folderpicker.h"
#include "synccontroller.h"

#include <KLocalizedString>

#include <QAbstractItemModelTester>
#include <QSignalSpy>
#include <QTest>

#include <memory>

/// The picker of the chosen folders (issue #58) against the fake daemon's
/// OneDrive: Documents (Work (Old), Taxes), Music, Photos (2019, 2020). What
/// it shows checked, partly checked or not, what a click changes, what it
/// says leaves this computer, and what Apply sends. Then the bind with
/// "Choose Folders…" (SyncController).
class FolderPickerTest : public QObject
{
    Q_OBJECT

private:
    std::unique_ptr<FakeDaemon> m_daemon;
    FakeSync *m_fake = nullptr;

    void startFake()
    {
        m_daemon = std::make_unique<FakeDaemon>();
        m_fake = m_daemon->sync;
        m_fake->folder->set({{QStringLiteral("Path"), QStringLiteral("/home/u/OneDrive")},
                             {QStringLiteral("State"), QStringLiteral("ready")},
                             {QStringLiteral("Source"), QStringLiteral("onedrive")}});
        m_fake->drive = {{QStringLiteral("d"), QStringLiteral("Documents"), QString()},
                         {QStringLiteral("w"), QStringLiteral("Work"), QStringLiteral("d")},
                         {QStringLiteral("o"), QStringLiteral("Old"), QStringLiteral("w")},
                         {QStringLiteral("t"), QStringLiteral("Taxes"), QStringLiteral("d")},
                         {QStringLiteral("m"), QStringLiteral("Music"), QString()},
                         {QStringLiteral("p"), QStringLiteral("Photos"), QString()},
                         {QStringLiteral("p19"), QStringLiteral("2019"), QStringLiteral("p")},
                         {QStringLiteral("p20"), QStringLiteral("2020"), QStringLiteral("p")}};
        QVERIFY(m_daemon->start());
    }

    void choose(const QStringList &ids, bool rootFiles)
    {
        m_fake->everything = false;
        m_fake->chosen = ids;
        m_fake->rootFiles = rootFiles;
    }

    /// What the fake was asked about the bind and the selection, in order.
    QStringList bindCalls() const
    {
        QStringList calls;
        for (const QString &call : std::as_const(m_fake->calls)) {
            if (call.startsWith(QLatin1String("SetSelection")) || call.startsWith(QLatin1String("Register")) || call == QLatin1String("SyncEverything")) {
                calls << call;
            }
        }
        return calls;
    }

    static int rowOf(const FolderPicker &picker, const QString &name)
    {
        for (int row = 0; row < picker.rowCount(); ++row) {
            if (picker.data(picker.index(row), FolderPicker::NameRole).toString() == name) {
                return row;
            }
        }
        return -1;
    }

    static int check(const FolderPicker &picker, const QString &name)
    {
        const int row = rowOf(picker, name);
        return row < 0 ? -1 : picker.data(picker.index(row), FolderPicker::CheckRole).toInt();
    }

    void openPicker(FolderPicker &picker)
    {
        picker.open();
        QTRY_VERIFY(!picker.loading());
        QCOMPARE(picker.problem(), QString());
    }

private Q_SLOTS:
    void initTestCase()
    {
        KLocalizedString::setApplicationDomain("konedrive");
    }

    void cleanup()
    {
        if (m_daemon) {
            m_daemon->stop();
        }
        m_daemon.reset();
        m_fake = nullptr;
    }

    /// With no selection everything is checked and the tree cannot be clicked;
    /// "Sync everything" switched off starts from every folder of the root and
    /// the root's files, so nothing leaves until something is unchecked.
    void fromEverythingToAList()
    {
        startFake();
        FolderPicker picker(QDBusConnection::sessionBus(), fake::FirstAccount);
        QAbstractItemModelTester tester(&picker);
        openPicker(picker);
        QCOMPARE(picker.rowCount(), 3);
        QCOMPARE(picker.data(picker.index(0), FolderPicker::NameRole).toString(), QStringLiteral("Documents"));
        QVERIFY(picker.everything());
        QCOMPARE(check(picker, QStringLiteral("Music")), int(Qt::Checked));
        picker.setChecked(rowOf(picker, QStringLiteral("Music")), false);
        QVERIFY(!picker.modified());

        picker.setEverything(false);
        QCOMPARE(picker.chosenIds(), (QStringList{QStringLiteral("d"), QStringLiteral("m"), QStringLiteral("p")}));
        QVERIFY(picker.rootFiles());
        QVERIFY(picker.modified());
        QCOMPARE(picker.summary(), QString());

        picker.setChecked(rowOf(picker, QStringLiteral("Music")), false);
        QCOMPARE(check(picker, QStringLiteral("Music")), int(Qt::Unchecked));
        QCOMPARE(picker.summary(), QStringLiteral("Removed from this computer (it stays in OneDrive): Music"));
        picker.setRootFiles(false);
        QVERIFY(picker.summary().contains(QStringLiteral("The files directly in the root of OneDrive are removed")));

        QSignalSpy applied(&picker, &FolderPicker::applied);
        picker.apply();
        QTRY_COMPARE(applied.size(), 1);
        QVERIFY(m_fake->calls.contains(QStringLiteral("SetSelection:d,p:no-root-files")));
        QVERIFY(!picker.modified());

        // Back to everything: SyncEverything.
        picker.setEverything(true);
        picker.apply();
        QTRY_COMPARE(applied.size(), 2);
        QVERIFY(m_fake->calls.contains(QStringLiteral("SyncEverything")));
    }

    /// Branches are read as they open. Unchecking a folder inside a chosen one
    /// makes the chosen one give way to its other sub-folders, and it becomes
    /// partly checked: the files directly in it leave, which the summary says.
    void uncheckingInsideAChosenFolder()
    {
        startFake();
        choose({QStringLiteral("d")}, false);
        FolderPicker picker(QDBusConnection::sessionBus(), fake::FirstAccount);
        QAbstractItemModelTester tester(&picker);
        openPicker(picker);
        QVERIFY(!picker.everything());
        QVERIFY(!picker.rootFiles());
        QCOMPARE(check(picker, QStringLiteral("Documents")), int(Qt::Checked));
        QCOMPARE(check(picker, QStringLiteral("Music")), int(Qt::Unchecked));
        QVERIFY(picker.data(picker.index(0), FolderPicker::ExpandableRole).toBool());
        QVERIFY(!picker.data(picker.index(rowOf(picker, QStringLiteral("Music"))), FolderPicker::ExpandableRole).toBool());

        picker.toggle(rowOf(picker, QStringLiteral("Documents")));
        QTRY_COMPARE(picker.rowCount(), 5);
        QVERIFY(m_fake->calls.contains(QStringLiteral("FolderChildren:d")));
        QCOMPARE(rowOf(picker, QStringLiteral("Taxes")), 1);
        QCOMPARE(picker.data(picker.index(1), FolderPicker::DepthRole).toInt(), 1);
        QCOMPARE(check(picker, QStringLiteral("Work")), int(Qt::Checked));

        picker.setChecked(rowOf(picker, QStringLiteral("Work")), false);
        QCOMPARE(picker.chosenIds(), QStringList{QStringLiteral("t")});
        QCOMPARE(check(picker, QStringLiteral("Documents")), int(Qt::PartiallyChecked));
        QCOMPARE(check(picker, QStringLiteral("Work")), int(Qt::Unchecked));
        QCOMPARE(check(picker, QStringLiteral("Taxes")), int(Qt::Checked));
        QCOMPARE(picker.summary(),
                 QStringLiteral("Removed from this computer (it stays in OneDrive): Documents/Work\n"
                                "Only the chosen sub-folders of Documents are synced: the files directly in it are removed from this computer too."));

        // Closed, the branch's rows go; opened again, it is not read again.
        const auto asked = m_fake->calls.count(QStringLiteral("FolderChildren:d"));
        picker.toggle(0);
        QCOMPARE(picker.rowCount(), 3);
        picker.toggle(0);
        QCOMPARE(picker.rowCount(), 5);
        QCOMPARE(m_fake->calls.count(QStringLiteral("FolderChildren:d")), asked);

        QSignalSpy applied(&picker, &FolderPicker::applied);
        picker.apply();
        QTRY_COMPARE(applied.size(), 1);
        QVERIFY(m_fake->calls.contains(QStringLiteral("SetSelection:t:no-root-files")));
    }

    /// A chosen folder in a branch never opened counts by its path: its parent
    /// is partly checked; checking the parent replaces it, and unchecking the
    /// parent names it among what leaves.
    void aChosenFolderInABranchNeverOpened()
    {
        startFake();
        choose({QStringLiteral("p19")}, true);
        FolderPicker picker(QDBusConnection::sessionBus(), fake::FirstAccount);
        openPicker(picker);
        QCOMPARE(check(picker, QStringLiteral("Photos")), int(Qt::PartiallyChecked));

        picker.setChecked(rowOf(picker, QStringLiteral("Photos")), true);
        QCOMPARE(picker.chosenIds(), QStringList{QStringLiteral("p")});
        QCOMPARE(check(picker, QStringLiteral("Photos")), int(Qt::Checked));
        picker.setChecked(rowOf(picker, QStringLiteral("Photos")), false);
        QCOMPARE(picker.chosenIds(), QStringList());
        QCOMPARE(picker.summary(),
                 QStringLiteral("No folder is chosen: every folder of OneDrive is removed from this computer; they stay in OneDrive.\n"
                                "Removed from this computer (it stays in OneDrive): Photos/2019"));
    }

    /// LocalChanges: the daemon's paths and why are shown, nothing changed, and
    /// the picker stays as it was edited.
    void aRefusalIsShownAndChangesNothing()
    {
        startFake();
        choose({QStringLiteral("d"), QStringLiteral("m")}, true);
        m_fake->refuseSelection = QStringLiteral(
            "1 file(s) or folder(s) that would be removed from this computer exist only here; nothing was changed:\nMusic/link: a symbolic link, which is never uploaded");
        FolderPicker picker(QDBusConnection::sessionBus(), fake::FirstAccount);
        openPicker(picker);
        picker.setChecked(rowOf(picker, QStringLiteral("Music")), false);
        QSignalSpy applied(&picker, &FolderPicker::applied);
        picker.apply();
        QTRY_VERIFY(!picker.problem().isEmpty());
        QVERIFY(picker.problem().contains(QStringLiteral("Music/link: a symbolic link")));
        QVERIFY(applied.isEmpty());
        QVERIFY(!picker.applying());
        QVERIFY(picker.modified());
        QCOMPARE(m_fake->chosen, (QStringList{QStringLiteral("d"), QStringLiteral("m")}));
    }

    /// Review fix 8: a picker whose first read failed — the selection, or the root's
    /// folders — cannot be changed or applied, and keeps saying why: an empty list
    /// made from nothing read would take every folder off this computer.
    void aFailedReadSendsNothing()
    {
        startFake();
        m_fake->refuseChildren = true;
        FolderPicker picker(QDBusConnection::sessionBus(), fake::FirstAccount);
        picker.open();
        QTRY_VERIFY(!picker.loading());
        QVERIFY(!picker.problem().isEmpty());
        QVERIFY(!picker.ready());
        picker.setEverything(false);
        QVERIFY(picker.everything());
        picker.setRootFiles(false);
        picker.apply();
        QTest::qWait(100);
        QVERIFY(!picker.problem().isEmpty());
        QVERIFY(m_fake->calls.filter(QStringLiteral("SetSelection")).isEmpty());
        QVERIFY(!m_fake->calls.contains(QStringLiteral("SyncEverything")));

        // Read again, it works.
        m_fake->refuseChildren = false;
        openPicker(picker);
        QVERIFY(picker.ready());
    }

    /// Review fix 8: a list left with no folder is said in words before Apply, from
    /// everything and from a list of folders alike.
    void anEmptyListIsSaidInWords()
    {
        startFake();
        choose({QStringLiteral("m")}, true);
        FolderPicker picker(QDBusConnection::sessionBus(), fake::FirstAccount);
        openPicker(picker);
        picker.click(rowOf(picker, QStringLiteral("Music")));
        QVERIFY(picker.chosenIds().isEmpty());
        QVERIFY2(picker.summary().startsWith(QStringLiteral("No folder is chosen: every folder of OneDrive is removed from this computer")), qPrintable(picker.summary()));

        // From everything: the root's folders, all unchecked one by one.
        m_fake->everything = true;
        openPicker(picker);
        picker.setEverything(false);
        for (const QString &name : {QStringLiteral("Documents"), QStringLiteral("Music"), QStringLiteral("Photos")}) {
            picker.click(rowOf(picker, name));
        }
        QVERIFY2(picker.summary().startsWith(QStringLiteral("No folder is chosen")), qPrintable(picker.summary()));
    }

    /// Review fix 11: a click on a partly checked folder unchecks it — every chosen
    /// folder below it goes — and the next click checks it whole.
    void aClickOnAPartlyCheckedFolderUnchecksIt()
    {
        startFake();
        choose({QStringLiteral("p19"), QStringLiteral("m")}, true);
        FolderPicker picker(QDBusConnection::sessionBus(), fake::FirstAccount);
        openPicker(picker);
        QCOMPARE(check(picker, QStringLiteral("Photos")), int(Qt::PartiallyChecked));
        picker.click(rowOf(picker, QStringLiteral("Photos")));
        QCOMPARE(check(picker, QStringLiteral("Photos")), int(Qt::Unchecked));
        QCOMPARE(picker.chosenIds(), QStringList{QStringLiteral("m")});
        picker.click(rowOf(picker, QStringLiteral("Photos")));
        QCOMPARE(check(picker, QStringLiteral("Photos")), int(Qt::Checked));
        QCOMPARE(picker.chosenIds(), (QStringList{QStringLiteral("m"), QStringLiteral("p")}));
        picker.click(rowOf(picker, QStringLiteral("Photos")));
        QCOMPARE(check(picker, QStringLiteral("Photos")), int(Qt::Unchecked));
    }

    /// The window follows the selection's properties.
    void theControllerFollowsTheSelection()
    {
        startFake();
        SyncController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        QVERIFY(controller.syncsEverything());
        QVERIFY(controller.rootFiles());
        m_fake->everything = false;
        m_fake->chosen = {QStringLiteral("w"), QStringLiteral("gone")};
        m_fake->rootFiles = false;
        m_fake->folder->announceSelection();
        QTRY_VERIFY(!controller.syncsEverything());
        QVERIFY(!controller.rootFiles());
        QCOMPARE(controller.selectedFolders().size(), 2);
        const QVariantMap work = controller.selectedFolders().at(0).toMap();
        QCOMPARE(work.value(QStringLiteral("id")).toString(), QStringLiteral("w"));
        QCOMPARE(work.value(QStringLiteral("path")).toString(), QStringLiteral("Documents/Work"));
        QCOMPARE(controller.selectedFolders().at(1).toMap().value(QStringLiteral("path")).toString(), QString());
    }

    /// "Choose Folders…" at the bind: an empty selection, then Register; bound, the
    /// picker is due. A refused bind takes the empty selection back; one refused for
    /// want of the helper is tried again the same way.
    void bindingWithChooseFolders()
    {
        startFake();
        m_fake->folder->set({{QStringLiteral("Path"), QString()}, {QStringLiteral("State"), QStringLiteral("none")}, {QStringLiteral("Source"), QString()}});
        SyncController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());

        m_fake->refuseRegister = QStringLiteral("org.konedrive.Error.NotSignedIn");
        controller.chooseFolderAndFolders(QUrl::fromLocalFile(QStringLiteral("/home/u/OneDrive")));
        QTRY_VERIFY(m_fake->calls.contains(QStringLiteral("SyncEverything")));
        QCOMPARE(bindCalls(), (QStringList{QStringLiteral("SetSelection::no-root-files"), QStringLiteral("Register:/home/u/OneDrive"), QStringLiteral("SyncEverything")}));
        QTRY_VERIFY(!controller.actionError().isEmpty());
        QVERIFY(!controller.choosePending());
        QVERIFY(m_fake->everything);

        m_fake->calls.clear();
        m_fake->refuseRegister.clear();
        m_fake->helperMissing = true;
        controller.chooseFolderAndFolders(QUrl::fromLocalFile(QStringLiteral("/home/u/OneDrive")));
        QTRY_COMPARE(controller.pendingFolder(), QStringLiteral("/home/u/OneDrive"));
        QTRY_VERIFY(m_fake->calls.contains(QStringLiteral("SyncEverything")));
        QVERIFY(!controller.choosePending());

        m_fake->calls.clear();
        m_fake->helperMissing = false;
        controller.retryRegistration();
        QTRY_VERIFY(controller.choosePending());
        QCOMPARE(bindCalls(), (QStringList{QStringLiteral("SetSelection::no-root-files"), QStringLiteral("Register:/home/u/OneDrive")}));
        QVERIFY(controller.pendingFolder().isEmpty());
        QVERIFY(!m_fake->everything);
        QVERIFY(m_fake->chosen.isEmpty());

        // Review fix 7: with a folder bound, nothing is called — an empty list
        // would start taking everything off that folder.
        QTRY_COMPARE(controller.rootPath(), QStringLiteral("/home/u/OneDrive"));
        m_fake->calls.clear();
        m_fake->everything = true;
        controller.chooseFolderAndFolders(QUrl::fromLocalFile(QStringLiteral("/home/u/Other")));
        QTRY_VERIFY(controller.actionError().contains(QStringLiteral("/home/u/OneDrive")));
        QTest::qWait(100);
        QVERIFY(bindCalls().isEmpty());
        QVERIFY(m_fake->everything);

        // Forgotten, nothing is due any more.
        controller.forget();
        QTRY_VERIFY(!controller.choosePending());
    }
};

QTEST_MAIN(FolderPickerTest)

#include "folderpickertest.moc"
