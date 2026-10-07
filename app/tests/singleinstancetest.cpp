#include "fakedaemon.h"

#include <QDBusConnection>
#include <QDBusMessage>
#include <QProcess>
#include <QProcessEnvironment>
#include <QSignalSpy>
#include <QTemporaryDir>
#include <QTest>

#include <functional>

namespace
{
constexpr int StartMs = 30000;
constexpr int ExitMs = 15000;
const QByteArray Ready = "konedrive.app: ready, window hidden";
const QByteArray Shown = "konedrive.app: showing the window";
const QByteArray Hidden = "konedrive.app: hiding the window";
const QByteArray Launch = "konedrive.app: second launch";
}

/// KOneDrive is single-instance: a second launch shows the
/// running window and exits; a second launch with --background (a login
/// while the app already runs) leaves it hidden. Runs the real program on the
/// test's private bus, offscreen, with every XDG directory in a temporary one.
/// The test plays the system tray too (a StatusNotifierWatcher), so the
/// program's tray icon registers with it and can be clicked over D-Bus. Every
/// wait runs this process's event loop, which is what answers the program's
/// calls to that tray.
class SingleInstanceTest : public QObject
{
    Q_OBJECT

private:
    QTemporaryDir m_home;
    FakeTrayWatcher m_tray;
    QProcess m_first;
    QByteArray m_firstOutput;

    QProcessEnvironment environment() const
    {
        QProcessEnvironment env = QProcessEnvironment::systemEnvironment();
        env.insert(QStringLiteral("XDG_CONFIG_HOME"), m_home.filePath(QStringLiteral("config")));
        env.insert(QStringLiteral("XDG_CACHE_HOME"), m_home.filePath(QStringLiteral("cache")));
        env.insert(QStringLiteral("XDG_STATE_HOME"), m_home.filePath(QStringLiteral("state")));
        env.insert(QStringLiteral("XDG_DATA_HOME"), m_home.filePath(QStringLiteral("data")));
        env.insert(QStringLiteral("QT_QPA_PLATFORM"), QStringLiteral("offscreen"));
        env.insert(QStringLiteral("QML_DISABLE_DISK_CACHE"), QStringLiteral("1"));
        env.insert(QStringLiteral("QT_LOGGING_RULES"), QStringLiteral("konedrive.app.debug=true"));
        // Qt logs to the journal when stderr is not a terminal; the test reads stderr.
        env.insert(QStringLiteral("QT_FORCE_STDERR_LOGGING"), QStringLiteral("1"));
        return env;
    }

    void launch(QProcess &process, const QStringList &arguments)
    {
        process.setProcessEnvironment(environment());
        process.setProcessChannelMode(QProcess::MergedChannels);
        process.start(QStringLiteral(KONEDRIVE_PROGRAM), arguments);
        QSignalSpy started(&process, &QProcess::started);
        QVERIFY2(process.state() == QProcess::Running || started.wait(StartMs), qPrintable(process.errorString()));
    }

    /// Reads the first instance's output until `done` holds for it.
    bool waitForFirstUntil(const std::function<bool(const QByteArray &)> &done, int timeoutMs)
    {
        QDeadlineTimer deadline(timeoutMs);
        m_firstOutput += m_first.readAll();
        while (!done(m_firstOutput)) {
            QSignalSpy output(&m_first, &QProcess::readyReadStandardOutput);
            if (m_first.state() != QProcess::Running || !output.wait(int(qMax<qint64>(1, deadline.remainingTime())))) {
                m_firstOutput += m_first.readAll();
                return done(m_firstOutput);
            }
            m_firstOutput += m_first.readAll();
        }
        return true;
    }

    /// Reads the first instance's output until it holds `count` copies of `line`.
    bool waitForFirst(const QByteArray &line, int count, int timeoutMs)
    {
        return waitForFirstUntil(
            [&](const QByteArray &output) {
                return output.count(line) >= count;
            },
            timeoutMs);
    }

    /// Runs another instance to its end.
    void runSecond(const QStringList &arguments)
    {
        QProcess second;
        QSignalSpy finished(&second, &QProcess::finished);
        launch(second, arguments);
        const bool exited = second.state() == QProcess::NotRunning || finished.wait(ExitMs);
        const QByteArray output = second.readAll();
        if (!exited) {
            second.kill();
            second.waitForFinished();
        }
        QVERIFY2(exited, QByteArray("the second instance kept running: " + output).constData());
        QCOMPARE(second.exitStatus(), QProcess::NormalExit);
        QVERIFY2(second.exitCode() == 0, output.constData());
    }

private Q_SLOTS:
    void initTestCase()
    {
        QVERIFY(m_home.isValid());
        QVERIFY(m_tray.start(QDBusConnection::sessionBus()));
        launch(m_first, {QStringLiteral("--background")});
        QVERIFY2(waitForFirst(Ready, 1, StartMs), m_firstOutput.constData());
    }

    void cleanupTestCase()
    {
        m_first.terminate();
        QSignalSpy finished(&m_first, &QProcess::finished);
        if (m_first.state() != QProcess::NotRunning && !finished.wait(ExitMs)) {
            m_first.kill();
            m_first.waitForFinished();
        }
    }

    void aSecondLaunchShowsTheRunningWindow()
    {
        QCOMPARE(m_firstOutput.count(Shown), 0);
        runSecond({});
        QVERIFY2(waitForFirst(Shown, 1, ExitMs), m_firstOutput.constData());
        QCOMPARE(m_first.state(), QProcess::Running);
    }

    /// A --background launch hands over and exits without showing anything:
    /// the plain launch after it adds exactly one "showing".
    void aBackgroundLaunchLeavesItHidden()
    {
        const auto shownBefore = m_firstOutput.count(Shown);
        const auto launchesBefore = m_firstOutput.count(Launch);
        runSecond({QStringLiteral("--background")});
        runSecond({});
        // The first logs each launch before what it does about it. Once the
        // plain launch's "showing" (after the last launch line) is read, so is
        // anything the --background launch did before it.
        QVERIFY2(waitForFirst(Launch, launchesBefore + 2, ExitMs), m_firstOutput.constData());
        QVERIFY2(waitForFirstUntil(
                     [](const QByteArray &output) {
                         return output.indexOf(Shown, output.lastIndexOf(Launch)) >= 0;
                     },
                     ExitMs),
                 m_firstOutput.constData());
        QCOMPARE(m_firstOutput.count(Shown), shownBefore + 1);
    }

    /// With a tray, a click on the icon hides the shown window and the app
    /// keeps running; launching it again shows the window (review B4).
    void aTrayClickHidesTheWindowAndARelaunchShowsIt()
    {
        QTRY_VERIFY_WITH_TIMEOUT(!m_tray.items.isEmpty(), ExitMs);
        const QString item = m_tray.items.constLast();

        auto click = QDBusMessage::createMethodCall(item, QStringLiteral("/StatusNotifierItem"), QStringLiteral("org.kde.StatusNotifierItem"), QStringLiteral("Activate"));
        click << 0 << 0;
        QDBusConnection::sessionBus().asyncCall(click);
        QVERIFY2(waitForFirst(Hidden, m_firstOutput.count(Hidden) + 1, ExitMs), m_firstOutput.constData());
        QCOMPARE(m_first.state(), QProcess::Running);

        const auto shownBefore = m_firstOutput.count(Shown);
        runSecond({});
        QVERIFY2(waitForFirst(Shown, shownBefore + 1, ExitMs), m_firstOutput.constData());
    }
};

QTEST_GUILESS_MAIN(SingleInstanceTest)

#include "singleinstancetest.moc"
