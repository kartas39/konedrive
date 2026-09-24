#pragma once

#include <QObject>
#include <QString>

#include <functional>

class AccountController;
class SyncController;
class QTimer;

/// One summary of the account and the folder, for the window's status line
/// and the tray icon. It reads only what the controllers already
/// hold: the "checked N s ago" text is refreshed from a clock every 10 s
/// without any D-Bus traffic.
class AppStatus : public QObject
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

    AppStatus(AccountController *account, SyncController *sync, Clock clock = {}, QObject *parent = nullptr);

    QString state() const { return m_state; }
    QString iconName() const;
    QString text() const { return m_text; }
    QString attention() const { return m_attention; }

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
    Clock m_clock;
    QTimer *m_tick;
    QString m_state = QStringLiteral("offline");
    QString m_text;
    QString m_attention;
};
