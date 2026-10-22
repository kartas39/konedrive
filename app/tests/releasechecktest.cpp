#include "buildversion.h"
#include "notifier.h"
#include "releasecheck.h"

#include <KConfig>
#include <KConfigGroup>

#include <QFile>
#include <QSignalSpy>
#include <QStandardPaths>
#include <QTemporaryDir>
#include <QTest>

Q_DECLARE_METATYPE(ReleaseFinding)

/// Stands in for KNotification: nothing reaches the desktop.
class RecordingSink : public NotificationSink
{
public:
    void send(const Notice &notice) override { sent << notice; }
    QList<Notice> sent;
};

/// The newer release: how build versions are read and ordered, what a latest release is to
/// the build that runs, how GitHub's answer is read, and what the check shows and notifies.
/// No test asks GitHub: answers are handed to `takeAnswer`. konedriverc is under a temporary
/// XDG_CONFIG_HOME, so the user's own ~/.config is never touched.
class ReleaseCheckTest : public QObject
{
    Q_OBJECT

private:
    QTemporaryDir m_config;

    static QByteArray answer(const QString &tag)
    {
        return QStringLiteral("{\"tag_name\": \"%1\", \"name\": \"KOneDrive %2\", \"draft\": false, \"prerelease\": false, "
                              "\"html_url\": \"https://github.com/kartas39/konedrive/releases/tag/%1\"}")
            .arg(tag, tag.mid(1))
            .toUtf8();
    }

    static QString notifiedRelease()
    {
        KConfig config(QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation) + QStringLiteral("/konedriverc"), KConfig::SimpleConfig);
        return config.group(QStringLiteral("General")).readEntry("NotifiedRelease", QString());
    }

private Q_SLOTS:
    void init()
    {
        QVERIFY(m_config.isValid());
        qputenv("XDG_CONFIG_HOME", QFile::encodeName(m_config.path()));
        QFile::remove(m_config.path() + QStringLiteral("/konedriverc"));
    }

    void aVersionIsReadInEveryShapeABuildHas_data()
    {
        QTest::addColumn<QString>("text");
        QTest::addColumn<bool>("read");
        QTest::addColumn<QString>("shown");
        QTest::newRow("release") << "0.2.1" << true << "0.2.1";
        QTest::newRow("tag") << "v0.2.1" << true << "0.2.1";
        QTest::newRow("package") << "0.2.2-dev.241" << true << "0.2.2-dev.241";
        QTest::newRow("plain build") << "0.2.2-dev" << true << "0.2.2-dev";
        QTest::newRow("empty") << "" << false << "";
        QTest::newRow("two numbers") << "0.2" << false << "";
        QTest::newRow("other suffix") << "0.2.1-rc1" << false << "";
        QTest::newRow("RPM spelling") << "0.2.2~dev.241" << false << "";
        QTest::newRow("trailing space") << "0.2.1 " << false << "";
    }

    void aVersionIsReadInEveryShapeABuildHas()
    {
        QFETCH(QString, text);
        QFETCH(bool, read);
        QFETCH(QString, shown);
        const std::optional<BuildVersion> version = BuildVersion::parse(text);
        QCOMPARE(version.has_value(), read);
        if (version) {
            QCOMPARE(version->toString(), shown);
        }
    }

    void aBuildOnTheWayToAReleaseIsOlderThanItAndNewerThanTheOneBefore_data()
    {
        QTest::addColumn<QString>("older");
        QTest::addColumn<QString>("newer");
        QTest::newRow("patch") << "0.2.1" << "0.2.2";
        QTest::newRow("minor over patch") << "0.2.9" << "0.3.0";
        QTest::newRow("major over minor") << "0.9.9" << "1.0.0";
        QTest::newRow("numbers, not text") << "0.2.9" << "0.2.10";
        QTest::newRow("package below its release") << "0.2.2-dev.5" << "0.2.2";
        QTest::newRow("plain build below its release") << "0.2.2-dev" << "0.2.2";
        QTest::newRow("package above the release before") << "0.2.1" << "0.2.2-dev.5";
        QTest::newRow("packages by number") << "0.2.2-dev.9" << "0.2.2-dev.12";
    }

    void aBuildOnTheWayToAReleaseIsOlderThanItAndNewerThanTheOneBefore()
    {
        QFETCH(QString, older);
        QFETCH(QString, newer);
        const BuildVersion first = *BuildVersion::parse(older);
        const BuildVersion second = *BuildVersion::parse(newer);
        QVERIFY(first.isOlderThan(second));
        QVERIFY(!second.isOlderThan(first));
        QVERIFY(!first.isOlderThan(first));
    }

    void theLatestReleaseIsFoundNewerOnlyWhenItIs_data()
    {
        QTest::addColumn<QString>("running");
        QTest::addColumn<QString>("latest");
        QTest::addColumn<QString>("notified");
        QTest::addColumn<ReleaseFinding>("finding");
        QTest::newRow("the same release") << "0.2.1" << "v0.2.1" << "" << ReleaseFinding::NotNewer;
        QTest::newRow("an older release") << "0.2.2" << "v0.2.1" << "" << ReleaseFinding::NotNewer;
        QTest::newRow("a package of the next release") << "0.2.2-dev.5" << "v0.2.1" << "" << ReleaseFinding::NotNewer;
        QTest::newRow("a latest that cannot be read") << "0.2.1" << "nightly" << "" << ReleaseFinding::NotNewer;
        QTest::newRow("a build that cannot be read") << "custom" << "v0.2.2" << "" << ReleaseFinding::NotNewer;
        QTest::newRow("newer, never notified") << "0.2.1" << "v0.2.2" << "" << ReleaseFinding::NewerNotNotified;
        QTest::newRow("newer than a package") << "0.2.2-dev.5" << "v0.2.2" << "" << ReleaseFinding::NewerNotNotified;
        QTest::newRow("newer than a plain build") << "0.2.2-dev" << "v0.2.2" << "" << ReleaseFinding::NewerNotNotified;
        QTest::newRow("newer, an older one notified") << "0.2.1" << "v0.2.3" << "0.2.2" << ReleaseFinding::NewerNotNotified;
        QTest::newRow("newer, notified") << "0.2.1" << "v0.2.2" << "0.2.2" << ReleaseFinding::NewerNotified;
    }

    void theLatestReleaseIsFoundNewerOnlyWhenItIs()
    {
        QFETCH(QString, running);
        QFETCH(QString, latest);
        QFETCH(QString, notified);
        QFETCH(ReleaseFinding, finding);
        QCOMPARE(findRelease(BuildVersion::parse(running), BuildVersion::parse(latest), BuildVersion::parse(notified)), finding);
    }

    void gitHubsAnswerIsReadForItsTagAndPage()
    {
        const std::optional<LatestRelease> release = readLatestRelease(answer(QStringLiteral("v0.2.2")));
        QVERIFY(release);
        QCOMPARE(release->tag, QStringLiteral("v0.2.2"));
        QCOMPARE(release->page, QUrl(QStringLiteral("https://github.com/kartas39/konedrive/releases/tag/v0.2.2")));

        QVERIFY(!readLatestRelease(QByteArrayLiteral("<html>rate limited</html>")));
        QVERIFY(!readLatestRelease(QByteArrayLiteral("{\"message\": \"Not Found\"}")));
        QVERIFY(!readLatestRelease(QByteArrayLiteral("{\"tag_name\": \"v0.2.2\", \"html_url\": \"http://example.org/\"}")));
    }

    /// A newer release is shown and notified once: not again for the same answer, and not
    /// again once the window starts anew. A release that is not newer clears what was shown.
    void aNewerReleaseIsShownAndNotifiedOnce()
    {
        RecordingSink sink;
        {
            ReleaseCheck check(QStringLiteral("0.2.1"), &sink);
            QSignalSpy changed(&check, &ReleaseCheck::newerReleaseChanged);
            QVERIFY(check.takeAnswer(answer(QStringLiteral("v0.2.2"))));
            QCOMPARE(check.newerVersion(), QStringLiteral("0.2.2"));
            QCOMPARE(check.newerPage(), QUrl(QStringLiteral("https://github.com/kartas39/konedrive/releases/tag/v0.2.2")));
            QCOMPARE(changed.count(), 1);
            QCOMPARE(sink.sent.size(), 1);
            QCOMPARE(sink.sent.at(0).event, QStringLiteral("newerRelease"));
            QCOMPARE(sink.sent.at(0).title, QStringLiteral("KOneDrive 0.2.2 is out"));
            QCOMPARE(sink.sent.at(0).link, check.newerPage());
            QCOMPARE(notifiedRelease(), QStringLiteral("0.2.2"));

            QVERIFY(check.takeAnswer(answer(QStringLiteral("v0.2.2"))));
            QCOMPARE(sink.sent.size(), 1);
            QCOMPARE(changed.count(), 1);
        }

        // The window started anew: shown, not notified.
        ReleaseCheck check(QStringLiteral("0.2.1"), &sink);
        QVERIFY(check.takeAnswer(answer(QStringLiteral("v0.2.2"))));
        QCOMPARE(check.newerVersion(), QStringLiteral("0.2.2"));
        QCOMPARE(sink.sent.size(), 1);

        // An answer that cannot be read changes nothing.
        QVERIFY(!check.takeAnswer(QByteArrayLiteral("{}")));
        QCOMPARE(check.newerVersion(), QStringLiteral("0.2.2"));

        // The next one is notified.
        QVERIFY(check.takeAnswer(answer(QStringLiteral("v0.2.3"))));
        QCOMPARE(check.newerVersion(), QStringLiteral("0.2.3"));
        QCOMPARE(sink.sent.size(), 2);
        QCOMPARE(notifiedRelease(), QStringLiteral("0.2.3"));

        // Not newer than this build: nothing is shown.
        ReleaseCheck updated(QStringLiteral("0.2.3"), &sink);
        QVERIFY(updated.takeAnswer(answer(QStringLiteral("v0.2.3"))));
        QVERIFY(updated.newerVersion().isEmpty());
        QCOMPARE(sink.sent.size(), 2);
    }
};

QTEST_GUILESS_MAIN(ReleaseCheckTest)
#include "releasechecktest.moc"
