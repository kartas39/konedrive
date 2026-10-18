#pragma once

#include "synctypes.h"

#include <QAbstractListModel>

/// The window's "Kept Both Versions" list: files changed here and in OneDrive, each with
/// the copy kept beside it (Conflicts.List()), newest first, the first `Shown` of
/// them; `total` counts them all. Rows are kept by the copy's path, and a
/// refresh applies what changed at once — one removal, one insertion and one
/// dataChanged, or else one reset — never row by row.
class ConflictModel : public QAbstractListModel
{
    Q_OBJECT
    /// The rows shown: at most `Shown`.
    Q_PROPERTY(int count READ count NOTIFY countChanged)
    /// Every conflict, shown or not.
    Q_PROPERTY(int total READ total NOTIFY countChanged)

public:
    enum Role {
        TimeRole = Qt::UserRole + 1,
        OriginalRole,
        CopyRole,
        NameRole,
        OriginalFolderRole,
        CopyFolderRole,
        CopyNameRole,
    };
    Q_ENUM(Role)

    /// How many rows the window shows; the rest are counted in `total`.
    static constexpr int Shown = 200;

    explicit ConflictModel(QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;
    QHash<int, QByteArray> roleNames() const override;

    int count() const { return static_cast<int>(m_rows.size()); }
    int total() const { return m_total; }

    void setConflicts(const KonedriveConflictList &conflicts);

Q_SIGNALS:
    void countChanged();

private:
    QList<KonedriveConflict> m_rows;
    int m_total = 0;
};
