#include "downloadprogresssettings.h"

#include <QFile>
#include <QSignalSpy>
#include <QStandardPaths>
#include <QTemporaryDir>
#include <QTest>

/// "Show download progress" (Ruling 3): konedriverc's [General]
/// group, the same way Autostart::StartAtLogin is stored, under a temporary
/// XDG_CONFIG_HOME so the user's own ~/.config is never touched.
class DownloadProgressSettingsTest : public QObject
{
    Q_OBJECT

private:
    QTemporaryDir m_config;

private Q_SLOTS:
    void init()
    {
        QVERIFY(m_config.isValid());
        qputenv("XDG_CONFIG_HOME", QFile::encodeName(m_config.path()));
        QCOMPARE(QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation), m_config.path());
    }

    void onByDefault()
    {
        DownloadProgressSettings settings;
        QVERIFY(settings.enabled());
    }

    void turningItOffPersistsAcrossInstances()
    {
        DownloadProgressSettings settings;
        QSignalSpy changed(&settings, &DownloadProgressSettings::enabledChanged);
        settings.setEnabled(false);
        QVERIFY(!settings.enabled());
        QCOMPARE(changed.count(), 1);

        DownloadProgressSettings restarted;
        QVERIFY(!restarted.enabled());

        restarted.setEnabled(true);
        DownloadProgressSettings restartedAgain;
        QVERIFY(restartedAgain.enabled());
    }

    void settingItToItsCurrentValueDoesNothing()
    {
        DownloadProgressSettings settings;
        QSignalSpy changed(&settings, &DownloadProgressSettings::enabledChanged);
        settings.setEnabled(true);
        QCOMPARE(changed.count(), 0);
    }
};

QTEST_GUILESS_MAIN(DownloadProgressSettingsTest)

#include "downloadprogresssettingstest.moc"
