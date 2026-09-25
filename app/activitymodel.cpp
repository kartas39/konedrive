#include "activitymodel.h"

#include "outboxmodel.h"

#include <KLocalizedString>

#include <QFileInfo>

ActivityFailure classifyFailure(const QString &kind, const QString &detail)
{
    if (kind == QLatin1String("upload-failed")) {
        return ActivityFailure::Upload;
    }
    const bool download = kind == QLatin1String("failed");
    const bool update = kind == QLatin1String("update-failed");
    if (!download && !update) {
        return ActivityFailure::None;
    }
    // The daemon's exact detail for ENOSPC, in either kind.
    if (detail == QLatin1String("not enough disk space")) {
        return ActivityFailure::DiskFull;
    }
    return update ? ActivityFailure::Update : ActivityFailure::Download;
}

bool isConflictCopy(const QString &original, const QString &other)
{
    return !original.isEmpty() && !other.isEmpty() && QFileInfo(original).path() == QFileInfo(other).path();
}

ActivityModel::ActivityModel(QObject *parent)
    : QAbstractListModel(parent)
{
}

int ActivityModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : count();
}

QString ActivityModel::describe(const QString &kind, const QString &detail)
{
    Q_UNUSED(detail) // shown beside the text, not folded into it
    if (kind == QLatin1String("downloaded")) {
        return i18nc("@info activity", "Downloaded");
    }
    if (kind == QLatin1String("freed")) {
        return i18nc("@info activity", "Freed up");
    }
    if (kind == QLatin1String("added")) {
        return i18nc("@info activity", "Added in OneDrive");
    }
    if (kind == QLatin1String("updated")) {
        return i18nc("@info activity", "Updated from OneDrive");
    }
    if (kind == QLatin1String("removed")) {
        return i18nc("@info activity", "Removed in OneDrive");
    }
    if (kind == QLatin1String("moved")) {
        return i18nc("@info activity", "Moved in OneDrive");
    }
    if (kind == QLatin1String("listed")) {
        return i18nc("@info activity", "Listed");
    }
    if (kind == QLatin1String("conflict")) {
        // A rescue; a copy (TextRole, isConflictCopy) has its own words.
        return i18nc("@info activity", "Your changed version was moved");
    }
    if (kind == QLatin1String("uploaded")) {
        return i18nc("@info activity", "Uploaded");
    }
    if (kind == QLatin1String("cloud-moved")) {
        return i18nc("@info activity", "Moved in OneDrive too");
    }
    if (kind == QLatin1String("cloud-deleted")) {
        return i18nc("@info activity", "Moved to OneDrive's recycle bin");
    }
    if (kind == QLatin1String("upload-failed")) {
        return i18nc("@info activity", "Could not be uploaded");
    }
    if (kind == QLatin1String("restored")) {
        return i18nc("@info activity", "Restored");
    }
    if (kind == QLatin1String("failed")) {
        return i18nc("@info activity", "Could not be downloaded");
    }
    if (kind == QLatin1String("update-failed")) {
        return i18nc("@info activity", "Could not update");
    }
    return kind;
}

QVariant ActivityModel::data(const QModelIndex &index, int role) const
{
    if (!checkIndex(index, CheckIndexOption::IndexIsValid | CheckIndexOption::ParentIsInvalid)) {
        return {};
    }
    const KonedriveActivity &row = m_rows.at(index.row());
    switch (role) {
    case Qt::DisplayRole:
    case NameRole: {
        const QString name = QFileInfo(row.path).fileName();
        return name.isEmpty() ? row.path : name;
    }
    case TimeRole:
        return row.time;
    case KindRole:
        return row.kind;
    case PathRole:
        return row.path;
    case FolderRole:
        return QFileInfo(row.path).path();
    case DetailRole:
        return row.detail;
    case TextRole:
        if (row.kind == QLatin1String("conflict") && isConflictCopy(row.path, row.detail)) {
            return i18nc("@info activity", "Changed on both sides: both versions kept");
        }
        return describe(row.kind, row.detail);
    case DetailTextRole:
        if (row.kind == QLatin1String("cloud-moved") && !row.detail.isEmpty()) {
            // Its detail is where it was.
            return i18nc("@info activity detail: where a moved file was", "was %1", row.detail);
        }
        return row.kind == QLatin1String("upload-failed") ? uploadReasonText(row.detail) : row.detail;
    case IconRole:
        if (row.kind == QLatin1String("conflict") && isConflictCopy(row.path, row.detail)) {
            return QStringLiteral("document-duplicate");
        }
        if (row.kind == QLatin1String("failed") || row.kind == QLatin1String("update-failed") || row.kind == QLatin1String("conflict")
            || row.kind == QLatin1String("upload-failed")) {
            return QStringLiteral("dialog-warning");
        }
        if (row.kind == QLatin1String("uploaded") || row.kind == QLatin1String("cloud-moved")) {
            return QStringLiteral("cloud-upload");
        }
        if (row.kind == QLatin1String("cloud-deleted")) {
            return QStringLiteral("edit-delete");
        }
        if (row.kind == QLatin1String("restored")) {
            return QStringLiteral("edit-undo");
        }
        if (row.kind == QLatin1String("freed")) {
            return QStringLiteral("edit-clear");
        }
        if (row.kind == QLatin1String("removed")) {
            return QStringLiteral("edit-delete");
        }
        if (row.kind == QLatin1String("listed")) {
            return QStringLiteral("view-list-details");
        }
        return QStringLiteral("cloud-download");
    }
    return {};
}

QHash<int, QByteArray> ActivityModel::roleNames() const
{
    return {
        {TimeRole, "time"},
        {KindRole, "kind"},
        {PathRole, "path"},
        {NameRole, "name"},
        {FolderRole, "folder"},
        {DetailRole, "detail"},
        {TextRole, "what"}, // not "text": a delegate's own property
        {IconRole, "iconName"},
        {DetailTextRole, "detailText"},
    };
}

void ActivityModel::setEvents(const KonedriveActivityList &newestFirst)
{
    const int before = count();
    beginResetModel();
    m_rows = newestFirst.mid(0, Capacity);
    endResetModel();
    if (count() != before) {
        Q_EMIT countChanged();
    }
}

void ActivityModel::prepend(const KonedriveActivity &event)
{
    const int before = count();
    beginInsertRows(QModelIndex(), 0, 0);
    m_rows.prepend(event);
    endInsertRows();
    if (count() > Capacity) {
        beginRemoveRows(QModelIndex(), Capacity, count() - 1);
        m_rows.resize(Capacity);
        endRemoveRows();
    }
    if (count() != before) {
        Q_EMIT countChanged();
    }
}
