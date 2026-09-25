#include "appstatus.h"

#include "accountsmodel.h"
#include "daemoncontroller.h"

#include <KLocalizedString>

#include <QStringList>

AppStatus::AppStatus(AccountsModel *accounts, QObject *parent)
    : QObject(parent)
    , m_accounts(accounts)
{
    // Every AccountStatus change, the clock's included, reaches the model's rows.
    connect(m_accounts, &QAbstractItemModel::dataChanged, this, &AppStatus::update);
    connect(m_accounts, &QAbstractItemModel::rowsInserted, this, &AppStatus::update);
    connect(m_accounts, &QAbstractItemModel::rowsRemoved, this, &AppStatus::update);
    connect(m_accounts, &QAbstractItemModel::rowsMoved, this, &AppStatus::update);
    connect(m_accounts, &QAbstractItemModel::modelReset, this, &AppStatus::update);
    connect(m_accounts->daemon(), &DaemonController::serviceAvailableChanged, this, &AppStatus::update);
    update();
}

int AppStatus::rank(const QString &state)
{
    if (state == QLatin1String("warning")) {
        return 0;
    }
    if (state == QLatin1String("syncing")) {
        return 2;
    }
    if (state == QLatin1String("ok")) {
        return 3;
    }
    return 1;
}

QString AppStatus::iconName() const
{
    return AccountStatus::iconFor(m_state);
}

AccountItem *AppStatus::onlyAccountNeedingAttention() const
{
    AccountItem *found = nullptr;
    for (AccountItem *item : m_accounts->items()) {
        if (item->status()->state() == QLatin1String("warning")) {
            if (found) {
                return nullptr;
            }
            found = item;
        }
    }
    return found;
}

void AppStatus::update()
{
    const QList<AccountItem *> &items = m_accounts->items();
    QString state = QStringLiteral("offline");
    QString toolTip;

    if (items.isEmpty()) {
        toolTip = m_accounts->daemon()->serviceAvailable() ? i18n("No OneDrive account yet") : i18n("The KOneDrive service is not running");
    } else if (items.size() == 1) {
        const AccountStatus *status = items.constFirst()->status();
        state = status->state();
        toolTip = status->text();
        if (!status->attention().isEmpty()) {
            toolTip += QLatin1Char('\n') + status->attention();
        }
    } else {
        state = QStringLiteral("ok");
        QStringList lines;
        for (const AccountItem *item : items) {
            const AccountStatus *status = item->status();
            if (rank(status->state()) < rank(state)) {
                state = status->state();
            }
            const QString line = status->attention().isEmpty() ? status->text() : status->attention();
            lines << i18nc("@info:tooltip one account's line: account label, its status", "%1 — %2", item->account()->label(), line);
        }
        toolTip = lines.join(QLatin1Char('\n'));
    }

    if (state == m_state && toolTip == m_toolTip) {
        return;
    }
    m_state = state;
    m_toolTip = toolTip;
    Q_EMIT changed();
}
