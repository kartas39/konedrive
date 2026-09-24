#pragma once

#include "synctypes.h"

#include <QAbstractListModel>

/// The window's "Conflicts" list: local versions the sync moved out of the
/// way (Sync1's Conflicts()), newest first. Rows are kept by the path the
/// file was moved to, so a refresh inserts and removes rows rather than
/// rebuilding the list.
class ConflictModel : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ count NOTIFY countChanged)

public:
    enum Role {
        TimeRole = Qt::UserRole + 1,
        OriginalRole,
        RescuedRole,
        NameRole,
        OriginalFolderRole,
        RescuedFolderRole,
    };
    Q_ENUM(Role)

    explicit ConflictModel(QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;
    QHash<int, QByteArray> roleNames() const override;

    int count() const { return static_cast<int>(m_rows.size()); }

    void setConflicts(const KonedriveConflictList &conflicts);

Q_SIGNALS:
    void countChanged();

private:
    QList<KonedriveConflict> m_rows;
};
