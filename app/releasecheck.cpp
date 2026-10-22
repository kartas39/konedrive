#include "releasecheck.h"

#include "notifier.h"
#include "trayicon.h"

#include <KConfig>
#include <KConfigGroup>
#include <KLocalizedString>

#include <QJsonDocument>
#include <QJsonObject>
#include <QNetworkAccessManager>
#include <QNetworkReply>
#include <QNetworkRequest>
#include <QStandardPaths>
#include <QTimer>

namespace
{
const QString ConfigName = QStringLiteral("konedriverc");
const char Group[] = "General";
const char Key[] = "NotifiedRelease";
const QString GitHubLatestRelease = QStringLiteral("https://api.github.com/repos/kartas39/konedrive/releases/latest");
constexpr int AskTimeoutMs = 30 * 1000;

QString configPath()
{
    return QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation) + QLatin1Char('/') + ConfigName;
}
}

std::optional<LatestRelease> readLatestRelease(const QByteArray &answer)
{
    const QJsonDocument document = QJsonDocument::fromJson(answer);
    if (!document.isObject()) {
        return std::nullopt;
    }
    const QJsonObject release = document.object();
    const QString tag = release.value(QLatin1String("tag_name")).toString();
    const QUrl page(release.value(QLatin1String("html_url")).toString());
    if (tag.isEmpty() || !page.isValid() || page.scheme() != QLatin1String("https")) {
        return std::nullopt;
    }
    return LatestRelease{tag, page};
}

ReleaseFinding findRelease(const std::optional<BuildVersion> &running,
                           const std::optional<BuildVersion> &latest,
                           const std::optional<BuildVersion> &notified)
{
    if (!running || !latest || !running->isOlderThan(*latest)) {
        return ReleaseFinding::NotNewer;
    }
    if (notified && !notified->isOlderThan(*latest)) {
        return ReleaseFinding::NewerNotified;
    }
    return ReleaseFinding::NewerNotNotified;
}

QUrl ReleaseCheck::latestReleaseUrl()
{
    const QString other = qEnvironmentVariable("KONEDRIVE_LATEST_RELEASE_URL");
    return QUrl(other.isEmpty() ? GitHubLatestRelease : other);
}

ReleaseCheck::ReleaseCheck(const QString &running, NotificationSink *sink, QObject *parent)
    : QObject(parent)
    , m_running(BuildVersion::parse(running))
    , m_runningText(running)
    , m_sink(sink)
{
    KConfig config(configPath(), KConfig::SimpleConfig);
    m_notified = config.group(QLatin1String(Group)).readEntry(Key, QString());
}

void ReleaseCheck::start(const QUrl &url)
{
    if (!m_running || m_timer) {
        return;
    }
    m_url = url;
    m_network = new QNetworkAccessManager(this);
    m_timer = new QTimer(this);
    m_timer->setSingleShot(true);
    connect(m_timer, &QTimer::timeout, this, &ReleaseCheck::ask);
    ask();
}

void ReleaseCheck::ask()
{
    QNetworkRequest request(m_url);
    // GitHub refuses a request without one.
    request.setHeader(QNetworkRequest::UserAgentHeader, QStringLiteral("KOneDrive/%1").arg(m_runningText));
    request.setRawHeader("Accept", "application/vnd.github+json");
    request.setTransferTimeout(AskTimeoutMs);
    QNetworkReply *reply = m_network->get(request);
    connect(reply, &QNetworkReply::finished, this, [this, reply] {
        reply->deleteLater();
        const int status = reply->attribute(QNetworkRequest::HttpStatusCodeAttribute).toInt();
        const bool taken = reply->error() == QNetworkReply::NoError && status == 200 && takeAnswer(reply->readAll());
        if (!taken) {
            qCDebug(KONEDRIVE_APP) << "no answer about the latest release:" << status << reply->errorString();
        }
        m_timer->start(taken ? DayMs : RetryMs);
    });
}

bool ReleaseCheck::takeAnswer(const QByteArray &answer)
{
    const std::optional<LatestRelease> release = readLatestRelease(answer);
    if (!release) {
        return false;
    }
    const std::optional<BuildVersion> latest = BuildVersion::parse(release->tag);
    switch (findRelease(m_running, latest, BuildVersion::parse(m_notified))) {
    case ReleaseFinding::NotNewer:
        setNewer(QString(), QUrl());
        break;
    case ReleaseFinding::NewerNotified:
        setNewer(latest->toString(), release->page);
        break;
    case ReleaseFinding::NewerNotNotified: {
        setNewer(latest->toString(), release->page);
        m_notified = latest->toString();
        KConfig config(configPath(), KConfig::SimpleConfig);
        config.group(QLatin1String(Group)).writeEntry(Key, m_notified);
        config.sync();
        Notice notice;
        notice.event = QStringLiteral("newerRelease");
        notice.title = i18nc("@title the version of a new release", "KOneDrive %1 is out", m_notified);
        notice.text = i18n("Its packages are on its release page.");
        notice.link = release->page;
        m_sink->send(notice);
        break;
    }
    }
    return true;
}

void ReleaseCheck::setNewer(const QString &version, const QUrl &page)
{
    if (m_newerVersion == version && m_newerPage == page) {
        return;
    }
    m_newerVersion = version;
    m_newerPage = page;
    Q_EMIT newerReleaseChanged();
}
