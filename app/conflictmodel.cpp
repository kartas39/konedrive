#include "conflictmodel.h"

#include "timetext.h"

#include <QDateTime>
#include <QFileInfo>
#include <QHash>
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
    case CopyRole:
        return row.copy;
    case OriginalFolderRole:
        return QFileInfo(row.original).path();
    case CopyFolderRole:
        return QFileInfo(row.copy).path();
    case CopyNameRole:
        return QFileInfo(row.copy).fileName();
    case TimeTextRole:
        return timeText(row.time, QDateTime::currentSecsSinceEpoch());
    }
    return {};
}

QVariantList ConflictModel::rows() const
{
    const QHash<int, QByteArray> roles = roleNames();
    QVariantList rows;
    rows.reserve(count());
    for (int row = 0; row < count(); ++row) {
        QVariantMap fields;
        for (auto it = roles.constBegin(); it != roles.constEnd(); ++it) {
            fields.insert(QString::fromLatin1(it.value()), data(index(row), it.key()));
        }
        rows << fields;
    }
    return rows;
}

QHash<int, QByteArray> ConflictModel::roleNames() const
{
    return {
        {TimeRole, "time"},
        {OriginalRole, "original"},
        {CopyRole, "copy"},
        {NameRole, "name"},
        {OriginalFolderRole, "originalFolder"},
        {CopyFolderRole, "copyFolder"},
        {CopyNameRole, "copyName"},
        {TimeTextRole, "timeText"},
    };
}

void ConflictModel::setConflicts(const KonedriveConflictList &conflicts)
{
    const int beforeCount = count();
    const int beforeTotal = m_total;

    // Newest first, and each moved file once: rows are keyed by that path.
    KonedriveConflictList wanted;
    QSet<QString> keys;
    KonedriveConflictList sorted = conflicts;
    std::stable_sort(sorted.begin(), sorted.end(), [](const KonedriveConflict &a, const KonedriveConflict &b) {
        return a.time > b.time;
    });
    for (const KonedriveConflict &conflict : std::as_const(sorted)) {
        if (!keys.contains(conflict.copy)) {
            keys.insert(conflict.copy);
            wanted << conflict;
        }
    }
    m_total = static_cast<int>(wanted.size());
    if (wanted.size() > Shown) {
        wanted.resize(Shown);
    }

    // One diff, never row by row. The rows that went, if they are one run,
    // go in one removal; the new ones, if they are one run, come in one
    // insertion; the rows kept take their new fields in one dataChanged. A
    // dismissed conflict is one removal (plus, past `Shown`, the next one
    // appended), new ones on top one insertion — the view keeps its place.
    // Anything else (a time that changed the order, several runs) is one
    // reset.
    QHash<QString, int> oldRow;
    for (int row = 0; row < count(); ++row) {
        oldRow.insert(m_rows.at(row).copy, row);
    }
    QSet<QString> newKeys;
    for (const KonedriveConflict &conflict : std::as_const(wanted)) {
        newKeys.insert(conflict.copy);
    }
    // One run of rows [first, first + length) in `rows` for which `in` holds, or none.
    const auto run = [](const KonedriveConflictList &rows, auto in, int &first, int &length) {
        first = -1;
        length = 0;
        int last = -1;
        for (int row = 0; row < rows.size(); ++row) {
            if (in(rows.at(row))) {
                if (first < 0) {
                    first = row;
                }
                last = row;
                ++length;
            }
        }
        return length == 0 || last - first + 1 == length;
    };
    int removeFirst = 0;
    int removeCount = 0;
    int insertFirst = 0;
    int insertCount = 0;
    const auto went = [&newKeys](const KonedriveConflict &row) {
        return !newKeys.contains(row.copy);
    };
    const auto isNew = [&oldRow](const KonedriveConflict &row) {
        return !oldRow.contains(row.copy);
    };
    bool oneDiff = run(m_rows, went, removeFirst, removeCount) && run(wanted, isNew, insertFirst, insertCount);
    // The rows kept must be in the same order on both sides.
    int firstChanged = -1;
    int lastChanged = -1;
    int keptOld = 0;
    for (int row = 0; oneDiff && row < wanted.size(); ++row) {
        const auto it = oldRow.constFind(wanted.at(row).copy);
        if (it == oldRow.constEnd()) {
            continue;
        }
        while (keptOld < count() && went(m_rows.at(keptOld))) {
            ++keptOld;
        }
        if (*it != keptOld) {
            oneDiff = false;
            break;
        }
        const KonedriveConflict &was = m_rows.at(keptOld++);
        const KonedriveConflict &is = wanted.at(row);
        if (was.time != is.time || was.original != is.original || was.kind != is.kind) {
            if (firstChanged < 0) {
                firstChanged = row;
            }
            lastChanged = row;
        }
    }

    if (!oneDiff) {
        beginResetModel();
        m_rows = wanted;
        endResetModel();
    } else {
        if (removeCount > 0) {
            beginRemoveRows(QModelIndex(), removeFirst, removeFirst + removeCount - 1);
            m_rows.remove(removeFirst, removeCount);
            endRemoveRows();
        }
        // What is left is `wanted` less its one new run, so that run goes
        // in where it stands in `wanted`.
        if (insertCount > 0) {
            beginInsertRows(QModelIndex(), insertFirst, insertFirst + insertCount - 1);
            m_rows = wanted;
            endInsertRows();
        }
        m_rows = wanted;
        if (firstChanged >= 0) {
            Q_EMIT dataChanged(index(firstChanged), index(lastChanged));
        }
    }

    if (count() != beforeCount || m_total != beforeTotal) {
        Q_EMIT countChanged();
    }
}
