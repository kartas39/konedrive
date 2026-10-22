#pragma once

#include <QDBusConnection>
#include <QDBusObjectPath>
#include <QObject>
#include <QString>

class QTimer;

/// The systemd user unit the daemon runs as (konedrived.service): started from the window
/// when the service is not running, and its journal read to say why it stopped. Any call to
/// the daemon starts the unit by itself (D-Bus activation), so a start asked here is one
/// that activation could not make: a unit that failed, or hit its start limit, is reset
/// first. The window's controllers pick the service up once it is on the bus; nothing here
/// waits for that. Never blocks the GUI thread.
class ServiceUnit : public QObject
{
    Q_OBJECT
    /// A start asked by start() runs: from the call until systemd says how its job ended.
    Q_PROPERTY(bool starting READ starting NOTIFY changed)
    /// Why the last start failed; empty when it did not, and once a new one is asked.
    Q_PROPERTY(QString startError READ startError NOTIFY changed)
    /// The unit's last journal lines, as readJournal() read them; empty before.
    Q_PROPERTY(QString journal READ journal NOTIFY journalChanged)

public:
    static const QString UnitName;
    /// How many journal lines readJournal() reads.
    static constexpr int JournalLines = 40;
    /// How long a start may run before it is taken as ended without word from systemd.
    static constexpr int StartTimeoutMs = 60000;

    explicit ServiceUnit(QObject *parent = nullptr);
    ServiceUnit(const QDBusConnection &bus, QObject *parent = nullptr);

    bool starting() const { return m_starting; }
    QString startError() const { return m_startError; }
    QString journal() const { return m_journal; }

    /// ResetFailedUnit, then StartUnit(UnitName, "replace") on the systemd manager of the
    /// session. A refusal, or a job that ends other than "done", lands in startError.
    Q_INVOKABLE void start();
    /// `journalctl --user -u konedrived.service`, the last JournalLines lines, into `journal`.
    Q_INVOKABLE void readJournal();

Q_SIGNALS:
    void changed();
    void journalChanged();

private Q_SLOTS:
    void onJobRemoved(uint id, const QDBusObjectPath &job, const QString &unit, const QString &result);

private:
    void finishStart(const QString &error);

    QDBusConnection m_bus;
    bool m_starting = false;
    QString m_startError;
    QString m_journal;
    /// The job of the start under way; empty when none is.
    QString m_job;
    QTimer *m_timeout;
};
