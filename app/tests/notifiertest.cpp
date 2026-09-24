#include "accountcontroller.h"
#include "fakedaemon.h"
#include "notifier.h"
#include "synccontroller.h"

#include <QSignalSpy>
#include <QTest>
#include <QTimer>

#include <memory>

namespace
{
const QString Root = QStringLiteral("/home/u/OneDrive");
}

/// Stands in for KNotification: nothing reaches the desktop.
class RecordingSink : public NotificationSink
{
public:
    void send(const Notice &notice) override { sent << notice; }
    QList<Notice> sent;
};

/// Which reports notify, and how often, from a fake
/// daemon on the private bus through the real controllers.
class NotifierTest : public QObject
{
    Q_OBJECT

private:
    std::unique_ptr<FakeDaemon> m_daemon;
    std::unique_ptr<AccountController> m_account;
    std::unique_ptr<SyncController> m_sync;
    RecordingSink m_sink;
    qint64 m_nowMs = 1000000;
    std::unique_ptr<Notifier> m_notifier;

    void start(const QString &accountState = QStringLiteral("signed-in"))
    {
        m_daemon = std::make_unique<FakeDaemon>();
        m_daemon->account->set({{QStringLiteral("State"), accountState}});
        m_daemon->sync->set({{QStringLiteral("RootPath"), Root}, {QStringLiteral("RootState"), QStringLiteral("ready")}});
        QVERIFY(m_daemon->start());
        m_account = std::make_unique<AccountController>();
        m_sync = std::make_unique<SyncController>();
        m_notifier = std::make_unique<Notifier>(m_account.get(), m_sync.get(), &m_sink, [this] {
            return m_nowMs;
        });
        QTRY_VERIFY(m_account->serviceAvailable() && m_sync->serviceAvailable());
        QTRY_COMPARE(m_account->state(), accountState);
    }

    /// Emits ActivityAdded from the fake and waits until the window has it.
    void report(const QString &kind, const QString &name, const QString &detail)
    {
        QSignalSpy arrived(m_sync.get(), &SyncController::activityAdded);
        m_daemon->sync->activity(1758700000, kind, Root + QLatin1Char('/') + name, detail);
        QVERIFY(arrived.wait(5000));
    }

private Q_SLOTS:
    void init()
    {
        m_sink.sent.clear();
        m_nowMs = 1000000;
    }

    void cleanup()
    {
        m_notifier.reset();
        m_sync.reset();
        m_account.reset();
        if (m_daemon) {
            m_daemon->stop();
        }
        m_daemon.reset();
    }

    /// Ordinary activity sends nothing.
    void everydayActivityIsQuiet()
    {
        start();
        for (const QString &kind : {QStringLiteral("downloaded"), QStringLiteral("freed"), QStringLiteral("added"), QStringLiteral("updated"),
                                    QStringLiteral("removed"), QStringLiteral("moved"), QStringLiteral("listed")}) {
            report(kind, QStringLiteral("a.txt"), QString());
        }
        QCOMPARE(m_sink.sent.size(), 0);
    }

    void aFullDiskNotifies()
    {
        start();
        report(QStringLiteral("failed"), QStringLiteral("big.iso"), QStringLiteral("not enough disk space"));
        QCOMPARE(m_sink.sent.size(), 1);
        QCOMPARE(m_sink.sent.first().event, QStringLiteral("diskFull"));
        QVERIFY2(m_sink.sent.first().text.contains(QStringLiteral("big.iso")), qPrintable(m_sink.sent.first().text));
    }

    void aFailedDownloadNotifies()
    {
        start();
        report(QStringLiteral("failed"), QStringLiteral("a.txt"), QStringLiteral("the connection was reset"));
        QCOMPARE(m_sink.sent.size(), 1);
        QCOMPARE(m_sink.sent.first().event, QStringLiteral("downloadFailed"));
        QVERIFY(m_sink.sent.first().text.contains(QStringLiteral("a.txt")));
        QVERIFY(m_sink.sent.first().text.contains(QStringLiteral("the connection was reset")));
    }

    /// A replacement that failed is its own kind, `update-failed`.
    void aFailedUpdateNotifies()
    {
        start();
        report(QStringLiteral("update-failed"), QStringLiteral("a.txt"), QStringLiteral("the connection was reset"));
        QCOMPARE(m_sink.sent.size(), 1);
        QCOMPARE(m_sink.sent.first().event, QStringLiteral("updateFailed"));
        QVERIFY(m_sink.sent.first().text.contains(QStringLiteral("a.txt")));
        QVERIFY(m_sink.sent.first().text.contains(QStringLiteral("the connection was reset")));
    }

    /// A full disk is a full disk, whether a download or an update ran into it.
    void aFullDiskDuringAnUpdateNotifiesAsAFullDisk()
    {
        start();
        report(QStringLiteral("update-failed"), QStringLiteral("big.iso"), QStringLiteral("not enough disk space"));
        QCOMPARE(m_sink.sent.size(), 1);
        QCOMPARE(m_sink.sent.first().event, QStringLiteral("diskFull"));
    }

    /// The words "could not be updated" in a download's reason do not make it an update.
    void aFailedDownloadIsNeverReadAsAnUpdate()
    {
        start();
        report(QStringLiteral("failed"), QStringLiteral("a.txt"), QStringLiteral("could not be updated: the connection was reset"));
        QCOMPARE(m_sink.sent.size(), 1);
        QCOMPARE(m_sink.sent.first().event, QStringLiteral("downloadFailed"));
    }

    /// "Your changed version of X was moved to Y", with "Show in Folder" on the moved file.
    void aConflictNotifiesWithWhereTheFileWent()
    {
        start();
        const QString rescued = QStringLiteral("/home/u/.local/share/konedrive/rescued/20260924T101500/doc.odt");
        report(QStringLiteral("conflict"), QStringLiteral("doc.odt"), rescued);
        QCOMPARE(m_sink.sent.size(), 1);
        const Notice &notice = m_sink.sent.first();
        QCOMPARE(notice.event, QStringLiteral("conflict"));
        QCOMPARE(notice.text, QStringLiteral("Your changed version of doc.odt was moved to ") + rescued);
        QCOMPARE(notice.showPath, rescued);
    }

    /// M3: past the cap, the daemon's own "and N more" conflict event (path
    /// = the root) is a count, not a rescued path.
    void aCappedConflictSummaryCountsRatherThanNamingAPath()
    {
        start();
        // report() always appends "/name" to Root; this event's path is the
        // root itself, so it is emitted directly.
        QSignalSpy arrived(m_sync.get(), &SyncController::activityAdded);
        m_daemon->sync->activity(1758700000, QStringLiteral("conflict"), Root, QStringLiteral("and 7 more"));
        QVERIFY(arrived.wait(5000));
        QCOMPARE(m_sink.sent.size(), 1);
        const Notice &notice = m_sink.sent.first();
        QCOMPARE(notice.event, QStringLiteral("conflict"));
        QCOMPARE(notice.text, QStringLiteral("7 more of your changed files were moved out of the way."));
        QCOMPARE(notice.showPath, QString());
    }

    void signingOutAfterBeingSignedInNotifies()
    {
        start();
        m_daemon->account->set({{QStringLiteral("State"), QStringLiteral("signed-out")}});
        QTRY_COMPARE(m_account->state(), QStringLiteral("signed-out"));
        QCOMPARE(m_sink.sent.size(), 1);
        QCOMPARE(m_sink.sent.first().event, QStringLiteral("signedOut"));
    }

    /// Starting signed out, or a sign-in given up, is not news.
    void neverSignedInIsQuiet()
    {
        start(QStringLiteral("signed-out"));
        m_daemon->account->set({{QStringLiteral("State"), QStringLiteral("signing-in")}});
        QTRY_COMPARE(m_account->state(), QStringLiteral("signing-in"));
        m_daemon->account->set({{QStringLiteral("State"), QStringLiteral("signed-out")}});
        QTRY_COMPARE(m_account->state(), QStringLiteral("signed-out"));
        QCOMPARE(m_sink.sent.size(), 0);
    }

    /// Signing out from the window is the user's own doing.
    void signingOutHereIsQuiet()
    {
        start();
        m_account->signOut();
        QTRY_COMPARE(m_account->state(), QStringLiteral("signed-out"));
        QCOMPARE(m_sink.sent.size(), 0);

        // The next sign-out that the user did not ask for notifies again.
        m_daemon->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        QTRY_COMPARE(m_account->state(), QStringLiteral("signed-in"));
        m_daemon->account->set({{QStringLiteral("State"), QStringLiteral("signed-out")}});
        QTRY_COMPARE(m_account->state(), QStringLiteral("signed-out"));
        QCOMPARE(m_sink.sent.size(), 1);
    }

    /// A daemon that restarts (a new owner of its name) hands the window its
    /// stored events and properties again: nothing is notified twice (review B4).
    void aDaemonRestartReplaysNothing()
    {
        start();
        report(QStringLiteral("failed"), QStringLiteral("a.txt"), QStringLiteral("timed out"));
        m_daemon->sync->log << KonedriveActivity{1758600000, QStringLiteral("update-failed"), Root + QStringLiteral("/b.txt"), QStringLiteral("timed out")}
                            << KonedriveActivity{1758500000, QStringLiteral("conflict"), Root + QStringLiteral("/c.txt"), QStringLiteral("/r/c.txt")};
        QCOMPARE(m_sink.sent.size(), 1);

        m_daemon->stop();
        QTRY_VERIFY(!m_sync->serviceAvailable());
        QVERIFY(m_daemon->start());
        QTRY_COMPARE(m_daemon->sync->calls.count(QStringLiteral("RecentActivity:50")), 2);
        QTRY_COMPARE(m_sync->activity()->count(), 3);
        QTRY_VERIFY(m_account->serviceAvailable());
        QCoreApplication::processEvents();
        QCOMPARE(m_account->state(), QStringLiteral("signed-in"));
        QCOMPARE(m_sink.sent.size(), 1);
    }

    /// At most one notification per kind in 10 s: the rest are counted and
    /// sent as one summary when the window ends; other kinds are separate.
    void oneNotificationPerKindInTenSeconds()
    {
        start();
        report(QStringLiteral("failed"), QStringLiteral("a.txt"), QStringLiteral("timed out"));
        QCOMPARE(m_sink.sent.size(), 1);
        QVERIFY(m_notifier->windowTimer()->isActive());
        QCOMPARE(m_notifier->windowTimer()->interval(), int(Notifier::WindowMs));

        m_nowMs += 2000;
        report(QStringLiteral("failed"), QStringLiteral("b.txt"), QStringLiteral("timed out"));
        m_nowMs += 2000;
        report(QStringLiteral("failed"), QStringLiteral("c.txt"), QStringLiteral("timed out"));
        report(QStringLiteral("failed"), QStringLiteral("big.iso"), QStringLiteral("not enough disk space"));
        QCOMPARE(m_sink.sent.size(), 2);
        QCOMPARE(m_sink.sent.at(1).event, QStringLiteral("diskFull"));

        // The window has not ended yet: nothing more.
        m_nowMs += 5000; // 9 s after the first
        m_notifier->flushDue();
        QCOMPARE(m_sink.sent.size(), 2);

        m_nowMs += 1000; // 10 s after the first
        m_notifier->flushDue();
        QCOMPARE(m_sink.sent.size(), 3);
        const Notice &summary = m_sink.sent.at(2);
        QCOMPARE(summary.event, QStringLiteral("downloadFailed"));
        QCOMPARE(summary.count, 2);
        QVERIFY2(summary.text.contains(QStringLiteral("2 more files")), qPrintable(summary.text));

        // The summary opened a new window: one more failure inside it waits.
        m_nowMs += 3000;
        report(QStringLiteral("failed"), QStringLiteral("d.txt"), QStringLiteral("timed out"));
        QCOMPARE(m_sink.sent.size(), 3);
        m_nowMs += 7000;
        m_notifier->flushDue();
        QCOMPARE(m_sink.sent.size(), 4);
        QCOMPARE(m_sink.sent.at(3).count, 1);

        // A quiet window closes; the next failure is sent at once.
        m_nowMs += 10000;
        m_notifier->flushDue();
        QCOMPARE(m_sink.sent.size(), 4);
        m_nowMs += 30000;
        report(QStringLiteral("failed"), QStringLiteral("e.txt"), QStringLiteral("timed out"));
        QCOMPARE(m_sink.sent.size(), 5);
        QCOMPARE(m_sink.sent.at(4).count, 1);
        QVERIFY(m_sink.sent.at(4).text.contains(QStringLiteral("e.txt")));
    }
};

QTEST_GUILESS_MAIN(NotifierTest)

#include "notifiertest.moc"
