#pragma once

#include "accountsmodel.h"

#include <QObject>
#include <QPointer>
#include <QString>

/// The account the window shows: the one chosen in the switcher, remembered
/// in konedriverc (`[General] CurrentAccount=<id>`). Until the remembered one
/// is there — or once it is gone — the first account; none while there is
/// none. The window's per-account pages bind to `account`, `sync` and
/// `status`, which are null when there is no account.
class CurrentAccount : public QObject
{
    Q_OBJECT
    Q_PROPERTY(AccountItem *item READ item NOTIFY changed)
    Q_PROPERTY(AccountController *account READ account NOTIFY changed)
    Q_PROPERTY(SyncController *sync READ sync NOTIFY changed)
    Q_PROPERTY(AccountStatus *status READ status NOTIFY changed)
    Q_PROPERTY(QString path READ path NOTIFY changed)
    /// An account other than this one needs attention (its state is "warning").
    Q_PROPERTY(bool othersNeedAttention READ othersNeedAttention NOTIFY othersNeedAttentionChanged)

public:
    explicit CurrentAccount(AccountsModel *model, QObject *parent = nullptr);

    AccountItem *item() const { return m_item; }
    AccountController *account() const;
    SyncController *sync() const;
    AccountStatus *status() const;
    QString path() const;
    bool othersNeedAttention() const { return m_othersNeedAttention; }

    /// Shows this account, and remembers the choice.
    Q_INVOKABLE void select(const QString &path);

Q_SIGNALS:
    void changed();
    void othersNeedAttentionChanged();

private:
    void reconsider();
    void setItem(AccountItem *item);
    void updateAttention();

    AccountsModel *m_model;
    QPointer<AccountItem> m_item;
    QString m_rememberedId;
    bool m_othersNeedAttention = false;
};
