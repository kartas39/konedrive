#pragma once

#include "synctypes.h"

#include <QAbstractListModel>

/// The downloads under way (Sync1's Transfers), one row per file. Rows are
/// kept by path: a download that goes on is updated in place, so a progress
/// bar in the window moves instead of being rebuilt.
class TransferModel : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ count NOTIFY countChanged)

public:
    enum Role {
        PathRole = Qt::UserRole + 1,
        NameRole,
        FolderRole,
        DoneRole,
        TotalRole,
        FractionRole,
    };
    Q_ENUM(Role)

    explicit TransferModel(QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;
    QHash<int, QByteArray> roleNames() const override;

    int count() const { return static_cast<int>(m_rows.size()); }

    /// Brings the rows in line with `transfers`: gone ones are removed, new
    /// ones appended, the rest updated where their numbers changed.
    void setTransfers(const KonedriveTransferList &transfers);

Q_SIGNALS:
    void countChanged();

private:
    QList<KonedriveTransfer> m_rows;
};
