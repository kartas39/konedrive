#pragma once

#include <QObject>
#include <QString>

#include <functional>

class AccountController;
class DaemonController;
class SyncController;
class QTimer;

/// One summary of one account and its folder, for the window's status line,
/// the account switcher and the tray. It reads only what the controllers
/// already hold: the "checked N s ago" text is refreshed from a clock every
/// 10 s without any D-Bus traffic.
class AccountStatus : public QObject
{
    Q_OBJECT
    /// "offline", "warning", "syncing" or "ok".
    Q_PROPERTY(QString state READ state NOTIFY changed)
    /// The icon for the state: state-offline, state-warning, state-sync, state-ok.
    Q_PROPERTY(QString iconName READ iconName NOTIFY changed)
    /// The status line: "Up to date · checked 20 s ago", "Listing your OneDrive: N items so far", the error…
    Q_PROPERTY(QString text READ text NOTIFY changed)
    /// Why the state is "warning" when the status line does not say it (a conflict, a failed update); else empty.
    Q_PROPERTY(QString attention READ attention NOTIFY changed)

public:
    /// Unix seconds.
    using Clock = std::function<qint64()>;
    static constexpr int TickMs = 10000;

    /// `daemon` says whether the helper, which serves every account, is there.
    AccountStatus(AccountController *account, SyncController *sync, DaemonController *daemon, Clock clock = {}, QObject *parent = nullptr);

    QString state() const { return m_state; }
    QString iconName() const;
    QString text() const { return m_text; }
    QString attention() const { return m_attention; }

    /// The icon for a state name.
    static QString iconFor(const QString &state);

    /// "20 s ago", "3 min ago"… for a unix time, against the clock.
    Q_INVOKABLE QString ago(qint64 unixSeconds) const;

public Q_SLOTS:
    /// Re-reads the clock; the 10 s timer calls it.
    void tick();

Q_SIGNALS:
    void changed();

private:
    void update();

    AccountController *m_account;
    SyncController *m_sync;
    DaemonController *m_daemon;
    Clock m_clock;
    QTimer *m_tick;
    QString m_state = QStringLiteral("offline");
    QString m_text;
    QString m_attention;
};
