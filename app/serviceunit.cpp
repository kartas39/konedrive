#include "serviceunit.h"

#include <KLocalizedString>

#include <QDBusMessage>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QProcess>
#include <QTimer>

const QString ServiceUnit::UnitName = QStringLiteral("konedrived.service");

namespace
{
const QString Systemd = QStringLiteral("org.freedesktop.systemd1");
const QString ManagerPath = QStringLiteral("/org/freedesktop/systemd1");
const QString Manager = QStringLiteral("org.freedesktop.systemd1.Manager");

QDBusMessage managerCall(const QString &method)
{
    return QDBusMessage::createMethodCall(Systemd, ManagerPath, Manager, method);
}
}

ServiceUnit::ServiceUnit(QObject *parent)
    : ServiceUnit(QDBusConnection::sessionBus(), parent)
{
}

ServiceUnit::ServiceUnit(const QDBusConnection &bus, QObject *parent)
    : QObject(parent)
    , m_bus(bus)
    , m_timeout(new QTimer(this))
{
    m_timeout->setSingleShot(true);
    m_timeout->setInterval(StartTimeoutMs);
    // No word from systemd: the start is taken as ended, and the window shows what the bus says.
    connect(m_timeout, &QTimer::timeout, this, [this] {
        finishStart(QString());
    });
    m_bus.connect(Systemd, ManagerPath, Manager, QStringLiteral("JobRemoved"), this, SLOT(onJobRemoved(uint, QDBusObjectPath, QString, QString)));
}

void ServiceUnit::start()
{
    if (m_starting) {
        return;
    }
    m_starting = true;
    m_startError.clear();
    m_job.clear();
    Q_EMIT changed();

    // systemd sends JobRemoved only once a client has subscribed; a unit that did not
    // fail refuses the reset, which changes nothing.
    m_bus.asyncCall(managerCall(QStringLiteral("Subscribe")));
    auto reset = managerCall(QStringLiteral("ResetFailedUnit"));
    reset << UnitName;
    m_bus.asyncCall(reset);
    auto startUnit = managerCall(QStringLiteral("StartUnit"));
    startUnit << UnitName << QStringLiteral("replace");
    auto *watcher = new QDBusPendingCallWatcher(m_bus.asyncCall(startUnit), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        const QDBusPendingReply<QDBusObjectPath> reply = *w;
        if (reply.isError()) {
            finishStart(i18n("The service could not be started: %1", reply.error().message()));
            return;
        }
        m_job = reply.value().path();
        m_timeout->start();
    });
}

void ServiceUnit::onJobRemoved(uint id, const QDBusObjectPath &job, const QString &unit, const QString &result)
{
    Q_UNUSED(id)
    if (!m_starting || unit != UnitName || job.path() != m_job) {
        return;
    }
    // "done" is a service on the bus (Type=dbus); anything else is a start that failed.
    finishStart(result == QLatin1String("done") ? QString() : i18n("The service stopped as it started (%1).", result));
}

void ServiceUnit::finishStart(const QString &error)
{
    m_timeout->stop();
    m_job.clear();
    m_starting = false;
    m_startError = error;
    Q_EMIT changed();
}

void ServiceUnit::readJournal()
{
    auto *process = new QProcess(this);
    connect(process, &QProcess::finished, this, [this, process](int exitCode, QProcess::ExitStatus status) {
        process->deleteLater();
        const QString out = QString::fromLocal8Bit(process->readAllStandardOutput()).trimmed();
        if (status != QProcess::NormalExit || exitCode != 0) {
            m_journal = i18n("The journal could not be read: %1", QString::fromLocal8Bit(process->readAllStandardError()).trimmed());
        } else {
            m_journal = out.isEmpty() ? i18n("The journal has nothing from the service.") : out;
        }
        Q_EMIT journalChanged();
    });
    connect(process, &QProcess::errorOccurred, this, [this, process](QProcess::ProcessError error) {
        if (error != QProcess::FailedToStart) {
            return;
        }
        process->deleteLater();
        m_journal = i18n("The journal could not be read: journalctl is not there.");
        Q_EMIT journalChanged();
    });
    process->start(QStringLiteral("journalctl"),
                   {QStringLiteral("--user"),
                    QStringLiteral("--unit"),
                    UnitName,
                    QStringLiteral("--lines"),
                    QString::number(JournalLines),
                    QStringLiteral("--no-pager"),
                    QStringLiteral("--output"),
                    QStringLiteral("short")});
}
