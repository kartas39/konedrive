#pragma once

#include <QObject>
#include <QString>

#include <functional>

class AccountController;
class SyncController;
class QTimer;

/// One summary of one account and its folder, for the window's status line,
/// the account switcher and the tray. The state is the daemon's (Folder.Overall);
/// the reason it gives chooses the words and what needs attention. The one state
/// decided here is "the service is not running". It reads only what the
/// controllers already hold: the "checked N s ago" text is refreshed from a clock
/// every 10 s without any D-Bus traffic.
class AccountStatus : public QObject
{
    Q_OBJECT
    /// "offline", "warning", "paused", "syncing" or "ok", as the daemon decided it; "offline"
    /// too while the service is not running, and for a state this build does not know.
    Q_PROPERTY(QString state READ state NOTIFY changed)
    /// The icon for the state: state-offline, state-warning, media-playback-pause, state-sync, state-ok.
    Q_PROPERTY(QString iconName READ iconName NOTIFY changed)
    /// The status line: "Up to date · checked 20 s ago" ("Up to date · live" while changes
    /// arrive through the notification socket), "Paused: metered connection" while the account
    /// holds back by itself, "Listing your OneDrive: N items so far", the error…
    Q_PROPERTY(QString text READ text NOTIFY changed)
    /// Why the state is "warning" when the status line does not say it (a conflict, a failed
    /// update), by the daemon's reason; else empty.
    Q_PROPERTY(QString attention READ attention NOTIFY changed)
    /// The folder's local scan, one line: "Checking local files: 1234 folders and 45678
    /// files, of about 50000 — started 2 min ago, after the switch to read-write", "Local
    /// files last checked 5 min ago (took 40 s)", "Local files not checked yet"; empty for a
    /// read-only folder.
    Q_PROPERTY(QString scanLine READ scanLine NOTIFY changed)

public:
    /// Unix seconds.
    using Clock = std::function<qint64()>;
    static constexpr int TickMs = 10000;

    AccountStatus(AccountController *account, SyncController *sync, Clock clock = {}, QObject *parent = nullptr);

    QString state() const { return m_state; }
    QString iconName() const;
    QString text() const { return m_text; }
    QString attention() const { return m_attention; }
    QString scanLine() const { return m_scanLine; }

    /// The icon for a state name.
    static QString iconFor(const QString &state);
    /// Why the account holds back by itself (HeldBack), as the Status page says it:
    /// "Paused: metered connection", "Paused: on battery"…
    static QString heldBackText(const QString &reason);

    /// "20 s ago", "3 min ago"… for a unix time, against the clock.
    Q_INVOKABLE QString ago(qint64 unixSeconds) const;
    /// A unix time to come: the time of day when it is today by the clock, else a short date and time.
    Q_INVOKABLE QString until(qint64 unixSeconds) const;

public Q_SLOTS:
    /// Re-reads the clock; the 10 s timer calls it.
    void tick();

Q_SIGNALS:
    void changed();

private:
    void update();

    AccountController *m_account;
    SyncController *m_sync;
    Clock m_clock;
    QTimer *m_tick;
    QString m_state = QStringLiteral("offline");
    QString m_text;
    QString m_attention;
    QString m_scanLine;
};
