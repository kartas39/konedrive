#include "activitymodel.h"
#include "conflictmodel.h"
#include "outboxmodel.h"
#include "transfermodel.h"

#include <QAbstractItemModelTester>
#include <QSignalSpy>
#include <QTest>

namespace
{
QVariant at(const QAbstractItemModel &model, int row, int role)
{
    return model.data(model.index(row, 0), role);
}

KonedriveActivity downloadedEvent(qint64 time, const QString &name)
{
    return {time, QStringLiteral("downloaded"), QStringLiteral("/home/u/OneDrive/") + name, QStringLiteral("1 KiB")};
}
}

class ModelsTest : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    /// A new download is inserted, one that goes on is updated in place (no
    /// reset, so its progress bar moves), a finished one is removed.
    void transfersInsertUpdateInPlaceAndRemove()
    {
        TransferModel model;
        QAbstractItemModelTester tester(&model, QAbstractItemModelTester::FailureReportingMode::QtTest);
        QSignalSpy inserted(&model, &QAbstractItemModel::rowsInserted);
        QSignalSpy changed(&model, &QAbstractItemModel::dataChanged);
        QSignalSpy removed(&model, &QAbstractItemModel::rowsRemoved);
        QSignalSpy reset(&model, &QAbstractItemModel::modelReset);

        model.setTransfers({{QStringLiteral("/d/a.iso"), 0, 400}, {QStringLiteral("/d/b.txt"), 1, 2}});
        QCOMPARE(model.count(), 2);
        QCOMPARE(inserted.count(), 2);
        QCOMPARE(at(model, 0, TransferModel::NameRole).toString(), QStringLiteral("a.iso"));
        QCOMPARE(at(model, 0, TransferModel::FolderRole).toString(), QStringLiteral("/d"));
        QCOMPARE(at(model, 1, TransferModel::FractionRole).toDouble(), 0.5);

        model.setTransfers({{QStringLiteral("/d/a.iso"), 100, 400}, {QStringLiteral("/d/b.txt"), 1, 2}});
        QCOMPARE(changed.count(), 1);
        QCOMPARE(changed.first().at(0).value<QModelIndex>().row(), 0);
        QCOMPARE(at(model, 0, TransferModel::DoneRole).toULongLong(), 100ULL);
        QCOMPARE(at(model, 0, TransferModel::FractionRole).toDouble(), 0.25);

        model.setTransfers({{QStringLiteral("/d/a.iso"), 400, 400}});
        QCOMPARE(model.count(), 1);
        QCOMPARE(removed.count(), 1);
        QCOMPARE(at(model, 0, TransferModel::PathRole).toString(), QStringLiteral("/d/a.iso"));
        QCOMPARE(reset.count(), 0);

        // A file of unknown size shows no progress rather than dividing by zero.
        model.setTransfers({{QStringLiteral("/d/c"), 5, 0}});
        QCOMPARE(at(model, 0, TransferModel::FractionRole).toDouble(), 0.0);
    }

    /// The write phase's kinds have words and icons; a failed upload's reason
    /// is shown in words; a conflict the daemon calls a copy is a copy.
    void uploadKindsAndCopies()
    {
        const QString root = QStringLiteral("/home/u/OneDrive");
        ActivityModel activity;
        activity.setEvents({{3, QStringLiteral("upload-failed"), root + QStringLiteral("/big.iso"), QStringLiteral("quota-exceeded")},
                            {2, QStringLiteral("conflict"), root + QStringLiteral("/Report.docx"), root + QStringLiteral("/Report-fedora.docx")},
                            {1, QStringLiteral("cloud-deleted"), root + QStringLiteral("/old.txt"), QStringLiteral("deleted in OneDrive; the placeholder here went too")}});
        QCOMPARE(at(activity, 0, ActivityModel::TextRole).toString(), QStringLiteral("Could not be uploaded"));
        QCOMPARE(at(activity, 0, ActivityModel::DetailTextRole).toString(), QStringLiteral("OneDrive is full: free some space in OneDrive."));
        QCOMPARE(at(activity, 0, ActivityModel::IconRole).toString(), QStringLiteral("dialog-warning"));
        QCOMPARE(at(activity, 1, ActivityModel::TextRole).toString(), QStringLiteral("Changed on both sides: both versions kept"));
        QCOMPARE(at(activity, 1, ActivityModel::IconRole).toString(), QStringLiteral("document-duplicate"));
        QCOMPARE(at(activity, 2, ActivityModel::IconRole).toString(), QStringLiteral("edit-delete"));
        QCOMPARE(uploadReasonText(QStringLiteral("other-device")), QStringLiteral("On another filesystem mounted inside the folder: never uploaded."));
        for (const QString &kind : {QStringLiteral("uploaded"), QStringLiteral("cloud-moved"), QStringLiteral("cloud-deleted"), QStringLiteral("restored")}) {
            QVERIFY2(ActivityModel::describe(kind, QString()) != kind, qPrintable(kind));
        }

        ConflictModel conflicts;
        conflicts.setConflicts({{2, root + QStringLiteral("/Report.docx"), root + QStringLiteral("/Report-fedora.docx"), QStringLiteral("copy")},
                                {1, root + QStringLiteral("/a.txt"), QStringLiteral("/home/u/.local/share/konedrive/rescued/1/a.txt"), QStringLiteral("rescued")}});
        QVERIFY(at(conflicts, 0, ConflictModel::IsCopyRole).toBool());
        QVERIFY(!at(conflicts, 1, ConflictModel::IsCopyRole).toBool());
    }

    /// Live events go on top; past 50 rows the oldest is removed.
    void activityPrependsAndDropsTheOldestPastFifty()
    {
        ActivityModel model;
        QAbstractItemModelTester tester(&model, QAbstractItemModelTester::FailureReportingMode::QtTest);
        for (int i = 0; i < ActivityModel::Capacity; ++i) {
            model.prepend(downloadedEvent(i, QStringLiteral("f%1").arg(i)));
        }
        QCOMPARE(model.count(), 50);
        QCOMPARE(at(model, 0, ActivityModel::NameRole).toString(), QStringLiteral("f49"));
        QCOMPARE(at(model, 49, ActivityModel::NameRole).toString(), QStringLiteral("f0"));

        QSignalSpy removed(&model, &QAbstractItemModel::rowsRemoved);
        model.prepend(downloadedEvent(50, QStringLiteral("f50")));
        QCOMPARE(model.count(), 50);
        QCOMPARE(removed.count(), 1);
        QCOMPARE(at(model, 0, ActivityModel::NameRole).toString(), QStringLiteral("f50"));
        QCOMPARE(at(model, 49, ActivityModel::NameRole).toString(), QStringLiteral("f1"));
        QCOMPARE(at(model, 0, ActivityModel::TimeRole).toLongLong(), 50LL);
        QCOMPARE(at(model, 0, ActivityModel::TextRole).toString(), QStringLiteral("Downloaded"));
    }

    /// A RecentActivity() answer replaces the rows, cut to 50.
    void activitySetEventsReplacesAndCaps()
    {
        ActivityModel model;
        QAbstractItemModelTester tester(&model, QAbstractItemModelTester::FailureReportingMode::QtTest);
        model.prepend(downloadedEvent(1, QStringLiteral("old")));
        KonedriveActivityList answer;
        for (int i = 70; i > 0; --i) {
            answer << downloadedEvent(100 + i, QStringLiteral("n%1").arg(i));
        }
        model.setEvents(answer);
        QCOMPARE(model.count(), 50);
        QCOMPARE(at(model, 0, ActivityModel::NameRole).toString(), QStringLiteral("n70"));
    }

    /// Conflicts are kept by the path the file was moved to, newest first: a
    /// refresh inserts what is new and removes what went.
    void conflictsInsertAndRemoveByRescuedPath()
    {
        ConflictModel model;
        QAbstractItemModelTester tester(&model, QAbstractItemModelTester::FailureReportingMode::QtTest);
        QSignalSpy inserted(&model, &QAbstractItemModel::rowsInserted);
        QSignalSpy removed(&model, &QAbstractItemModel::rowsRemoved);
        QSignalSpy reset(&model, &QAbstractItemModel::modelReset);

        model.setConflicts({{100, QStringLiteral("/d/a.txt"), QStringLiteral("/r/1/a.txt")}, {300, QStringLiteral("/d/c.txt"), QStringLiteral("/r/3/c.txt")}});
        QCOMPARE(model.count(), 2);
        QCOMPARE(at(model, 0, ConflictModel::RescuedRole).toString(), QStringLiteral("/r/3/c.txt"));
        QCOMPARE(at(model, 0, ConflictModel::NameRole).toString(), QStringLiteral("c.txt"));
        QCOMPARE(at(model, 0, ConflictModel::OriginalFolderRole).toString(), QStringLiteral("/d"));
        QCOMPARE(at(model, 0, ConflictModel::RescuedFolderRole).toString(), QStringLiteral("/r/3"));

        inserted.clear();
        model.setConflicts({{200, QStringLiteral("/d/b.txt"), QStringLiteral("/r/2/b.txt")},
                            {100, QStringLiteral("/d/a.txt"), QStringLiteral("/r/1/a.txt")},
                            {300, QStringLiteral("/d/c.txt"), QStringLiteral("/r/3/c.txt")}});
        QCOMPARE(inserted.count(), 1);
        QCOMPARE(inserted.first().at(1).toInt(), 1);
        QCOMPARE(at(model, 1, ConflictModel::OriginalRole).toString(), QStringLiteral("/d/b.txt"));

        model.setConflicts({{100, QStringLiteral("/d/a.txt"), QStringLiteral("/r/1/a.txt")}});
        QCOMPARE(model.count(), 1);
        QCOMPARE(removed.count(), 2);
        QCOMPARE(at(model, 0, ConflictModel::TimeRole).toLongLong(), 100LL);
        QCOMPARE(reset.count(), 0);
    }

    /// A reply that names one moved file twice lists it once, and the next
    /// refresh cannot run past the end of the rows (review B8).
    void conflictsWithADuplicatePathListItOnce()
    {
        ConflictModel model;
        QAbstractItemModelTester tester(&model, QAbstractItemModelTester::FailureReportingMode::QtTest);
        const KonedriveConflict a{100, QStringLiteral("/d/a.txt"), QStringLiteral("/r/1/a.txt")};
        const KonedriveConflict b{200, QStringLiteral("/d/b.txt"), QStringLiteral("/r/2/b.txt")};
        model.setConflicts({a, a});
        QCOMPARE(model.count(), 1);
        model.setConflicts({a});
        QCOMPARE(model.count(), 1);
        model.setConflicts({b, a, b});
        QCOMPARE(model.count(), 2);
        QCOMPARE(at(model, 0, ConflictModel::RescuedRole).toString(), b.rescued);
        QCOMPARE(at(model, 1, ConflictModel::RescuedRole).toString(), a.rescued);
    }

    /// Which failures are which (notifications): the kind says
    /// download (`failed`) or update (`update-failed`); the exact detail
    /// "not enough disk space" makes either a full disk. Wording says nothing else.
    void classifiesFailures()
    {
        const QString diskFull = QStringLiteral("not enough disk space");
        const QString reset = QStringLiteral("the connection was reset");
        QCOMPARE(classifyFailure(QStringLiteral("failed"), diskFull), ActivityFailure::DiskFull);
        QCOMPARE(classifyFailure(QStringLiteral("update-failed"), diskFull), ActivityFailure::DiskFull);
        QCOMPARE(classifyFailure(QStringLiteral("update-failed"), reset), ActivityFailure::Update);
        QCOMPARE(classifyFailure(QStringLiteral("update-failed"), QString()), ActivityFailure::Update);
        QCOMPARE(classifyFailure(QStringLiteral("failed"), reset), ActivityFailure::Download);
        QCOMPARE(classifyFailure(QStringLiteral("failed"), QString()), ActivityFailure::Download);
        // A download's reason that happens to say "could not be updated" is still a download.
        QCOMPARE(classifyFailure(QStringLiteral("failed"), QStringLiteral("could not be updated: ") + reset), ActivityFailure::Download);
        QCOMPARE(classifyFailure(QStringLiteral("updated"), QString()), ActivityFailure::None);
        QCOMPARE(classifyFailure(QStringLiteral("conflict"), QStringLiteral("/r/1/a.txt")), ActivityFailure::None);
    }

    /// The "Recent" list says what each failure was.
    void activityDescribesFailures()
    {
        ActivityModel model;
        model.prepend({1, QStringLiteral("failed"), QStringLiteral("/d/a.txt"), QStringLiteral("the connection was reset")});
        model.prepend({2, QStringLiteral("update-failed"), QStringLiteral("/d/b.txt"), QStringLiteral("the connection was reset")});
        QCOMPARE(at(model, 0, ActivityModel::TextRole).toString(), QStringLiteral("Could not update"));
        QCOMPARE(at(model, 0, ActivityModel::IconRole).toString(), QStringLiteral("dialog-warning"));
        QCOMPARE(at(model, 1, ActivityModel::TextRole).toString(), QStringLiteral("Could not be downloaded"));
    }
};

QTEST_GUILESS_MAIN(ModelsTest)

#include "modelstest.moc"
