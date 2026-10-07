#include "transfermodel.h"

#include <QFileInfo>
#include <QSet>

TransferModel::TransferModel(QObject *parent)
    : QAbstractListModel(parent)
{
}

int TransferModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : count();
}

QVariant TransferModel::data(const QModelIndex &index, int role) const
{
    if (!checkIndex(index, CheckIndexOption::IndexIsValid | CheckIndexOption::ParentIsInvalid)) {
        return {};
    }
    const KonedriveTransfer &row = m_rows.at(index.row());
    switch (role) {
    case Qt::DisplayRole:
    case NameRole:
        return QFileInfo(row.path).fileName();
    case PathRole:
        return row.path;
    case FolderRole:
        return QFileInfo(row.path).path();
    case DoneRole:
        return row.done;
    case TotalRole:
        return row.total;
    case FractionRole:
        return row.total > 0 ? qMin(1.0, double(row.done) / double(row.total)) : 0.0;
    }
    return {};
}

QHash<int, QByteArray> TransferModel::roleNames() const
{
    return {
        {PathRole, "path"},
        {NameRole, "name"},
        {FolderRole, "folder"},
        {DoneRole, "done"},
        {TotalRole, "total"},
        {FractionRole, "fraction"},
    };
}

void TransferModel::setTransfers(const KonedriveTransferList &transfers)
{
    const int before = count();

    QSet<QString> wanted;
    for (const KonedriveTransfer &transfer : transfers) {
        wanted.insert(transfer.path);
    }
    for (int row = count() - 1; row >= 0; --row) {
        if (!wanted.contains(m_rows.at(row).path)) {
            beginRemoveRows(QModelIndex(), row, row);
            m_rows.removeAt(row);
            endRemoveRows();
        }
    }

    for (const KonedriveTransfer &transfer : transfers) {
        const auto it = std::find_if(m_rows.begin(), m_rows.end(), [&transfer](const KonedriveTransfer &row) {
            return row.path == transfer.path;
        });
        if (it == m_rows.end()) {
            beginInsertRows(QModelIndex(), count(), count());
            m_rows.append(transfer);
            endInsertRows();
            continue;
        }
        if (it->done != transfer.done || it->total != transfer.total) {
            *it = transfer;
            const QModelIndex changed = index(static_cast<int>(std::distance(m_rows.begin(), it)));
            Q_EMIT dataChanged(changed, changed, {DoneRole, TotalRole, FractionRole});
        }
    }

    if (count() != before) {
        Q_EMIT countChanged();
    }
}
