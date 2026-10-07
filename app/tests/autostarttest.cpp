#include "autostart.h"

#include <QDir>
#include <QFile>
#include <QSignalSpy>
#include <QStandardPaths>
#include <QTemporaryDir>
#include <QTest>

namespace
{
const QString Program = QStringLiteral("/opt/konedrive/bin/konedrive");
}

/// "Start at login", under a temporary XDG_CONFIG_HOME: the
/// user's own ~/.config/autostart is never touched.
class AutostartTest : public QObject
{
    Q_OBJECT

private:
    QTemporaryDir m_config;

    QString entry() const { return m_config.filePath(QStringLiteral("autostart/org.konedrive.KOneDrive.desktop")); }

    static QByteArray read(const QString &path)
    {
        QFile file(path);
        return file.open(QIODevice::ReadOnly) ? file.readAll() : QByteArray();
    }

private Q_SLOTS:
    void init()
    {
        QVERIFY(m_config.isValid());
        qputenv("XDG_CONFIG_HOME", QFile::encodeName(m_config.path()));
        QCOMPARE(QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation), m_config.path());
        QVERIFY(QDir(m_config.path()).removeRecursively());
        QVERIFY(QDir().mkpath(m_config.path()));
    }

    void theEntryLivesUnderXdgConfigHome()
    {
        QCOMPARE(Autostart::entryPath(), entry());
    }

    void turningItOnWritesTheEntryAndOffRemovesIt()
    {
        Autostart autostart(Program);
        QVERIFY(!autostart.enabled());
        QSignalSpy changed(&autostart, &Autostart::enabledChanged);

        autostart.setEnabled(true);
        QVERIFY(autostart.enabled());
        QCOMPARE(changed.count(), 1);
        const QByteArray desktop = read(entry());
        QVERIFY2(desktop.startsWith("[Desktop Entry]\n"), desktop.constData());
        QVERIFY2(desktop.contains("\nType=Application\n"), desktop.constData());
        QVERIFY2(desktop.contains("\nExec=/opt/konedrive/bin/konedrive --background\n"), desktop.constData());

        autostart.setEnabled(false);
        QVERIFY(!autostart.enabled());
        QVERIFY(!QFile::exists(entry()));
        QCOMPARE(changed.count(), 2);
    }

    /// A program path with a space is quoted, as the Desktop Entry spec asks.
    void aProgramPathWithASpaceIsQuoted()
    {
        Autostart autostart(QStringLiteral("/home/Jo Doe/.local/bin/konedrive"));
        autostart.setEnabled(true);
        const QByteArray desktop = read(entry());
        QVERIFY2(desktop.contains("\nExec=\"/home/Jo Doe/.local/bin/konedrive\" --background\n"), desktop.constData());
    }

    /// A literal % is written %% (in Exec=, % starts a field code).
    void aPercentInTheProgramPathIsEscaped()
    {
        Autostart autostart(QStringLiteral("/opt/100%/konedrive"));
        autostart.setEnabled(true);
        const QByteArray desktop = read(entry());
        QVERIFY2(desktop.contains("\nExec=/opt/100%%/konedrive --background\n"), desktop.constData());
    }

    /// An entry that cannot be written leaves the switch off, says why, and
    /// tells the switch (which the user just flipped on) to go back.
    void aFailedWriteLeavesItOffAndSaysWhy()
    {
        QFile blocker(m_config.filePath(QStringLiteral("autostart"))); // a file where the directory goes
        QVERIFY(blocker.open(QIODevice::WriteOnly));
        blocker.close();

        Autostart autostart(Program);
        QSignalSpy changed(&autostart, &Autostart::enabledChanged);
        autostart.setEnabled(true);
        QVERIFY(!autostart.enabled());
        QVERIFY(!autostart.error().isEmpty());
        QCOMPARE(changed.count(), 1);

        QVERIFY(QFile::remove(blocker.fileName()));
        autostart.setEnabled(true);
        QVERIFY(autostart.enabled());
        QCOMPARE(autostart.error(), QString());
    }

    /// An entry that cannot be removed leaves the switch on and says why.
    void aFailedRemoveLeavesItOnAndSaysWhy()
    {
        Autostart autostart(Program);
        autostart.setEnabled(true);
        const QString dir = m_config.filePath(QStringLiteral("autostart"));
        QVERIFY(QFile::setPermissions(dir, QFileDevice::ReadOwner | QFileDevice::ExeOwner));
        QSignalSpy changed(&autostart, &Autostart::enabledChanged);
        autostart.setEnabled(false);
        const bool stillOn = autostart.enabled();
        const QString error = autostart.error();
        QVERIFY(QFile::setPermissions(dir, QFileDevice::ReadOwner | QFileDevice::WriteOwner | QFileDevice::ExeOwner));
        QVERIFY(stillOn);
        QVERIFY(!error.isEmpty());
        QCOMPARE(changed.count(), 1);
    }

    /// The first run turns it on, once; after that the user's choice stands,
    /// even across a restart.
    void theFirstRunTurnsItOnOnce()
    {
        {
            Autostart autostart(Program);
            autostart.applyFirstRunDefault();
            QVERIFY(autostart.enabled());
            QVERIFY(QFile::exists(entry()));
            autostart.setEnabled(false);
        }
        Autostart restarted(Program);
        restarted.applyFirstRunDefault();
        QVERIFY(!restarted.enabled());
        QVERIFY(!QFile::exists(entry()));
    }

    /// An entry removed elsewhere (System Settings → Autostart) reads as off,
    /// and a later start does not bring it back.
    void theEntryIsTheTruth()
    {
        Autostart autostart(Program);
        autostart.applyFirstRunDefault();
        QVERIFY(QFile::remove(entry()));
        QVERIFY(!autostart.enabled());
        Autostart restarted(Program);
        restarted.applyFirstRunDefault();
        QVERIFY(!restarted.enabled());
    }
};

QTEST_GUILESS_MAIN(AutostartTest)

#include "autostarttest.moc"
