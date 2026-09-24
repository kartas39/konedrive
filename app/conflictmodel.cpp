#include "conflictmodel.h"

#include <QFileInfo>
#include <QSet>

#include <algorithm>

ConflictModel::ConflictModel(QObject *parent)
    : QAbstractListModel(parent)
{
}

int ConflictModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : count();
}

QVariant ConflictModel::data(const QModelIndex &index, int role) const
{
    if (!checkIndex(index, CheckIndexOption::IndexIsValid | CheckIndexOption::ParentIsInvalid)) {
        return {};
    }
    const KonedriveConflict &row = m_rows.at(index.row());
    switch (role) {
    case Qt::DisplayRole:
    case NameRole:
        return QFileInfo(row.original).fileName();
    case TimeRole:
        return row.time;
    case OriginalRole:
        return row.original;
    case RescuedRole:
        return row.rescued;
    case OriginalFolderRole:
        return QFileInfo(row.original).path();
    case RescuedFolderRole:
        return QFileInfo(row.rescued).path();
    }
    return {};
}

QHash<int, QByteArray> ConflictModel::roleNames() const
{
    return {
        {TimeRole, "time"},
        {OriginalRole, "original"},
        {RescuedRole, "rescued"},
        {NameRole, "name"},
        {OriginalFolderRole, "originalFolder"},
        {RescuedFolderRole, "rescuedFolder"},
    };
}

void ConflictModel::setConflicts(const KonedriveConflictList &conflicts)
{
    const int before = count();

    // Newest first, and each moved file once: rows are keyed by that path,
    // and a key seen twice would put the walk below out of step.
    KonedriveConflictList wanted;
    QSet<QString> keys;
    KonedriveConflictList sorted = conflicts;
    std::stable_sort(sorted.begin(), sorted.end(), [](const KonedriveConflict &a, const KonedriveConflict &b) {
        return a.time > b.time;
    });
    for (const KonedriveConflict &conflict : std::as_const(sorted)) {
        if (!keys.contains(conflict.rescued)) {
            keys.insert(conflict.rescued);
            wanted << conflict;
        }
    }

    for (int row = count() - 1; row >= 0; --row) {
        if (!keys.contains(m_rows.at(row).rescued)) {
            beginRemoveRows(QModelIndex(), row, row);
            m_rows.removeAt(row);
            endRemoveRows();
        }
    }

    // What is left should be in `wanted`'s order already; if a time changed
    // and it is not, start over rather than insert a row twice.
    QSet<QString> kept;
    for (const KonedriveConflict &row : std::as_const(m_rows)) {
        kept.insert(row.rescued);
    }
    QStringList keptInWantedOrder;
    for (const KonedriveConflict &conflict : std::as_const(wanted)) {
        if (kept.contains(conflict.rescued)) {
            keptInWantedOrder << conflict.rescued;
        }
    }
    for (int row = 0; row < count(); ++row) {
        if (m_rows.at(row).rescued != keptInWantedOrder.at(row)) {
            beginResetModel();
            m_rows = wanted;
            endResetModel();
            if (count() != before) {
                Q_EMIT countChanged();
            }
            return;
        }
    }

    // Walk both, inserting what is new where it belongs.
    for (int row = 0; row < wanted.size(); ++row) {
        const KonedriveConflict &conflict = wanted.at(row);
        if (row < count() && m_rows.at(row).rescued == conflict.rescued) {
            if (m_rows.at(row).time != conflict.time || m_rows.at(row).original != conflict.original) {
                m_rows[row] = conflict;
                Q_EMIT dataChanged(index(row), index(row));
            }
            continue;
        }
        beginInsertRows(QModelIndex(), row, row);
        m_rows.insert(row, conflict);
        endInsertRows();
    }

    if (count() != before) {
        Q_EMIT countChanged();
    }
}
