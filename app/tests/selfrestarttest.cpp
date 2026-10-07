#include "fakedaemon.h"

#include <QDBusConnection>
#include <QFile>
#include <QProcess>
#include <QProcessEnvironment>
#include <QSignalSpy>
#include <QTemporaryDir>
#include <QTest>

namespace
{
constexpr int StartMs = 30000;
const QByteArray Ready = "konedrive.app: ready, window hidden";
const QByteArray Kept = "konedrive.app: the daemon is another build; this program is still the one installed";
const QByteArray Restarting = "konedrive.app: restarting as the installed program";
}

/// A newer package installed under a running window: once the daemon is back
/// as another build and the window's own program file has been replaced, the
/// window starts the installed program in its place — the same process, hidden
/// as it was. With the file untouched it stays as it is. Runs a copy of the
/// real program on the test's private bus, offscreen, with every XDG directory
/// in a temporary one; the test plays the daemon and the system tray.
class SelfRestartTest : public QObject
{
    Q_OBJECT

private:
    QTemporaryDir m_home;
    FakeTrayWatcher m_tray;
    FakeDaemon m_daemon;
    QProcess m_window;
    QByteArray m_output;

    QString program() const
    {
        return m_home.filePath(QStringLiteral("konedrive"));
    }

    /// Puts a fresh copy of the program at `to`, executable.
    static bool copyProgram(const QString &to)
    {
        return QFile::copy(QStringLiteral(KONEDRIVE_PROGRAM), to)
            && QFile::setPermissions(to, QFile::ReadOwner | QFile::WriteOwner | QFile::ExeOwner);
    }

    /// Reads the window's output until it holds `count` copies of `line`.
    bool waitFor(const QByteArray &line, int count)
    {
        QDeadlineTimer deadline(StartMs);
        m_output += m_window.readAll();
        while (m_output.count(line) < count) {
            QSignalSpy output(&m_window, &QProcess::readyReadStandardOutput);
            if (m_window.state() != QProcess::Running || !output.wait(int(qMax<qint64>(1, deadline.remainingTime())))) {
                m_output += m_window.readAll();
                break;
            }
            m_output += m_window.readAll();
        }
        return m_output.count(line) >= count;
    }

    /// The daemon goes and comes back as this build.
    void daemonComesBackAs(const QString &version, const QString &commit)
    {
        m_daemon.stop();
        m_daemon.manager->setBuild(version, commit);
        QVERIFY(m_daemon.start());
    }

private Q_SLOTS:
    void cleanupTestCase()
    {
        m_window.terminate();
        QSignalSpy finished(&m_window, &QProcess::finished);
        if (m_window.state() != QProcess::NotRunning && !finished.wait(StartMs)) {
            m_window.kill();
            m_window.waitForFinished();
        }
        m_daemon.stop();
    }

    void theWindowRestartsOnceItsProgramIsReplaced()
    {
        QVERIFY(m_home.isValid());
        QVERIFY(m_tray.start(QDBusConnection::sessionBus()));
        QVERIFY(m_daemon.start());
        QVERIFY(copyProgram(program()));

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
        m_window.setProcessEnvironment(env);
        m_window.setProcessChannelMode(QProcess::MergedChannels);
        m_window.start(program(), {QStringLiteral("--background")});
        QVERIFY2(m_window.waitForStarted(StartMs), qPrintable(m_window.errorString()));
        QVERIFY2(waitFor(Ready, 1), m_output.constData());
        const qint64 pid = m_window.processId();

        // Another daemon, the same program file: a window built by hand stays.
        daemonComesBackAs(QStringLiteral("9.9.9"), QStringLiteral("1111111111111111111111111111111111111111"));
        QVERIFY2(waitFor(Kept, 1), m_output.constData());
        QCOMPARE(m_output.count(Restarting), 0);

        // An update: a new file moved over the program's, then the daemon back.
        const QString fresh = m_home.filePath(QStringLiteral("konedrive.new"));
        QVERIFY(copyProgram(fresh));
        QVERIFY(::rename(QFile::encodeName(fresh).constData(), QFile::encodeName(program()).constData()) == 0);
        daemonComesBackAs(QStringLiteral("9.9.10"), QStringLiteral("2222222222222222222222222222222222222222"));
        QVERIFY2(waitFor(Restarting, 1), m_output.constData());
        // The installed program, in the same process, hidden as the window was,
        // and alone on the bus: a second "ready" means it took the name again.
        QVERIFY2(waitFor(Ready, 2), m_output.constData());
        QCOMPARE(m_window.state(), QProcess::Running);
        QCOMPARE(m_window.processId(), pid);
        QCOMPARE(m_output.count(Restarting), 1);
    }
};

QTEST_GUILESS_MAIN(SelfRestartTest)

#include "selfrestarttest.moc"
