#pragma once

#include "buildversion.h"

#include <QObject>
#include <QPointer>
#include <QString>
#include <QUrl>

#include <optional>

class NotificationSink;
class QNetworkAccessManager;
class QNetworkReply;
class QTimer;

/// What GitHub answers as the latest release of KOneDrive: its tag (`vX.Y.Z`) and its page.
struct LatestRelease {
    QString tag;
    QUrl page;
};

/// What the latest release is to the build that runs.
enum class ReleaseFinding {
    /// Not newer, or a version that cannot be read: nothing is said.
    NotNewer,
    /// Newer, and notified before: the foot of the sidebar says it, and nothing else.
    NewerNotified,
    /// Newer, and not notified yet: the foot says it, and one notification goes to the release's page.
    NewerNotNotified,
};

/// GitHub's answer for `releases/latest` (its `tag_name` and `html_url`); nothing for any
/// other answer.
std::optional<LatestRelease> readLatestRelease(const QByteArray &answer);

/// What the release `latest` is to the build `running`, `notified` being the release that was
/// last notified.
ReleaseFinding findRelease(const std::optional<BuildVersion> &running,
                           const std::optional<BuildVersion> &latest,
                           const std::optional<BuildVersion> &notified);

/// The window's check for a newer release of KOneDrive on GitHub, and what it found, for the
/// foot of the sidebar. Once started it asks for the latest release at once and then once a day;
/// an ask that gets no answer, or one that cannot be read, says nothing, changes nothing and is
/// asked again an hour later. The first time a release is found newer than this build, one
/// notification says so; that release is kept under `NotifiedRelease` in konedriverc's [General]
/// group, so it is not notified again once the window starts anew.
///
/// Whether it asks at all is the setting "Check for new versions once a day", kept under
/// `CheckForNewVersions` in the same group, on by default. Turned off, the ask under way is
/// dropped, nothing more is asked and no newer release is shown; turned on again, it asks at once.
class ReleaseCheck : public QObject
{
    Q_OBJECT
    /// Whether the check asks GitHub: the setting, as konedriverc keeps it.
    Q_PROPERTY(bool enabled READ enabled WRITE setEnabled NOTIFY enabledChanged)
    /// The newer release's version, `0.2.2`; empty while none is known or the check is off.
    Q_PROPERTY(QString newerVersion READ newerVersion NOTIFY newerReleaseChanged)
    /// KOneDrive's releases on GitHub, where every release's packages are: where the foot of the
    /// sidebar leads.
    Q_PROPERTY(QUrl releasesPage READ releasesPage CONSTANT)

public:
    static constexpr int DayMs = 24 * 60 * 60 * 1000;
    static constexpr int RetryMs = 60 * 60 * 1000;

    /// GitHub's latest release of KOneDrive, or the address `KONEDRIVE_LATEST_RELEASE_URL`
    /// names: the tests start the program with one that leads nowhere, so that they never
    /// reach GitHub.
    static QUrl latestReleaseUrl();

    /// KOneDrive's releases on GitHub.
    static QUrl releasesPage();

    /// `running` is this build's version, as the window shows it (KONEDRIVE_VERSION); the
    /// notification goes to `sink`.
    ReleaseCheck(const QString &running, NotificationSink *sink, QObject *parent = nullptr);

    /// Asks `url` now, when the check is on, and from then on by itself. A build whose version
    /// cannot be read never asks.
    void start(const QUrl &url);
    /// Takes an answer of GitHub's, as an ask does once it is back: false when it cannot be read.
    bool takeAnswer(const QByteArray &answer);

    bool enabled() const { return m_enabled; }
    /// Turns the check on or off, and keeps it in konedriverc.
    void setEnabled(bool enabled);
    QString newerVersion() const { return m_newerVersion; }

Q_SIGNALS:
    void enabledChanged();
    void newerReleaseChanged();

private:
    void ask();
    void setNewer(const QString &version);

    std::optional<BuildVersion> m_running;
    QString m_runningText;
    NotificationSink *m_sink;
    bool m_enabled;
    /// The release notified last, as konedriverc keeps it; empty before the first.
    QString m_notified;
    QString m_newerVersion;
    QUrl m_url;
    QNetworkAccessManager *m_network = nullptr;
    QTimer *m_timer = nullptr;
    /// The ask under way, dropped when the check is turned off.
    QPointer<QNetworkReply> m_reply;
};
