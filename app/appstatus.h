#pragma once

#include <QObject>
#include <QString>

class AccountItem;
class AccountsModel;

/// The whole app's state, for the tray: the worst state across the accounts
/// — needs attention ("warning"), then signed out ("offline"), then syncing,
/// then synced ("ok") — and a tooltip with one line per account. With no
/// account it is "offline". It reads only the accounts' AccountStatus.
class AppStatus : public QObject
{
    Q_OBJECT
    /// "offline", "warning", "syncing" or "ok".
    Q_PROPERTY(QString state READ state NOTIFY changed)
    Q_PROPERTY(QString iconName READ iconName NOTIFY changed)
    /// One account: its status line, and what needs attention on the next
    /// line. Several: "<label> — <status line>" per account, in order, the
    /// attention in place of the status line when there is some.
    Q_PROPERTY(QString toolTip READ toolTip NOTIFY changed)

public:
    explicit AppStatus(AccountsModel *accounts, QObject *parent = nullptr);

    QString state() const { return m_state; }
    QString iconName() const;
    QString toolTip() const { return m_toolTip; }
    AccountsModel *accounts() const { return m_accounts; }

    /// The account needing attention when exactly one does; else null.
    AccountItem *onlyAccountNeedingAttention() const;

    /// Lower is worse: warning 0, offline 1, syncing 2, ok 3.
    static int rank(const QString &state);

Q_SIGNALS:
    void changed();

private:
    void update();

    AccountsModel *m_accounts;
    QString m_state = QStringLiteral("offline");
    QString m_toolTip;
};
