#include "currentaccount.h"

#include "accountsmodel.h"

#include <KConfig>
#include <KConfigGroup>

#include <QStandardPaths>

namespace
{
const QString ConfigName = QStringLiteral("konedriverc");
const char Group[] = "General";
const char Key[] = "CurrentAccount";

QString configPath()
{
    return QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation) + QLatin1Char('/') + ConfigName;
}
}

CurrentAccount::CurrentAccount(AccountsModel *model, QObject *parent)
    : QObject(parent)
    , m_model(model)
{
    KConfig config(configPath(), KConfig::SimpleConfig);
    m_rememberedId = config.group(QLatin1String(Group)).readEntry(Key, QString());

    connect(m_model, &QAbstractItemModel::rowsInserted, this, &CurrentAccount::reconsider);
    connect(m_model, &QAbstractItemModel::rowsRemoved, this, &CurrentAccount::reconsider);
    connect(m_model, &QAbstractItemModel::rowsMoved, this, &CurrentAccount::reconsider);
    connect(m_model, &QAbstractItemModel::modelReset, this, &CurrentAccount::reconsider);
    connect(m_model, &QAbstractItemModel::dataChanged, this, &CurrentAccount::updateAttention);
    connect(m_model, &AccountsModel::accountAdded, this, &CurrentAccount::select);
    reconsider();
}

AccountController *CurrentAccount::account() const
{
    return m_item ? m_item->account() : nullptr;
}

SyncController *CurrentAccount::sync() const
{
    return m_item ? m_item->sync() : nullptr;
}

AccountStatus *CurrentAccount::status() const
{
    return m_item ? m_item->status() : nullptr;
}

QString CurrentAccount::path() const
{
    return m_item ? m_item->path() : QString();
}

void CurrentAccount::select(const QString &path)
{
    AccountItem *item = m_model->find(path);
    const QString id = item ? item->id() : path.section(QLatin1Char('/'), -1);
    if (id != m_rememberedId) {
        m_rememberedId = id;
        KConfig config(configPath(), KConfig::SimpleConfig);
        config.group(QLatin1String(Group)).writeEntry(Key, m_rememberedId);
        config.sync();
    }
    if (item) {
        setItem(item);
    }
}

void CurrentAccount::reconsider()
{
    // The remembered account whenever it is there (the rows arrive one by
    // one at startup), else the one shown if it is still there, else the first.
    AccountItem *next = nullptr;
    for (AccountItem *item : m_model->items()) {
        if (item->id() == m_rememberedId) {
            next = item;
        }
    }
    if (!next && m_item && m_model->indexOf(m_item->path()) >= 0) {
        next = m_item;
    }
    if (!next) {
        next = m_model->at(0);
    }
    setItem(next);
    updateAttention();
}

void CurrentAccount::setItem(AccountItem *item)
{
    if (m_item == item) {
        return;
    }
    m_item = item;
    Q_EMIT changed();
    updateAttention();
}

void CurrentAccount::updateAttention()
{
    bool others = false;
    for (const AccountItem *item : m_model->items()) {
        if (item != m_item && item->status()->state() == QLatin1String("warning")) {
            others = true;
        }
    }
    if (others == m_othersNeedAttention) {
        return;
    }
    m_othersNeedAttention = others;
    Q_EMIT othersNeedAttentionChanged();
}
