#pragma once

#include "synctypes.h"

#include <QAbstractListModel>

/// What an outbox row's reason, an `upload-failed` event's detail or a
/// NotUploaded() reason means, in the window's words: the same meanings as
/// konedrivectl's `upload_reason_text`, pointing at the window instead of
/// commands. A reason it does not know is kept as the daemon wrote it.
QString uploadReasonText(const QString &reason);

/// The window's "Waiting to upload" list: Sync1's Outbox(), oldest first, at
/// most DisplayLimit rows shown; `heldCount` and `total` count every row.
class OutboxModel : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ count NOTIFY changed)
    /// Every row the daemon listed, shown or not.
    Q_PROPERTY(int total READ total NOTIFY changed)
    /// Removals the mass-delete guard holds (state "held").
    Q_PROPERTY(int heldCount READ heldCount NOTIFY changed)

public:
    static constexpr int DisplayLimit = 100;

    enum Role {
        SeqRole = Qt::UserRole + 1,
        KindRole,
        PathRole,
        NameRole,
        FolderRole,
        StateRole,
        StateTextRole,
        DoneRole,
        TotalRole,
        FractionRole,
        ReasonRole,
        WhyRole,
        NextTryRole,
        IconRole,
    };
    Q_ENUM(Role)

    explicit OutboxModel(QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;
    QHash<int, QByteArray> roleNames() const override;

    int count() const { return static_cast<int>(m_rows.size()); }
    int total() const { return m_total; }
    int heldCount() const { return m_held; }

    /// Replaces the rows with `rows` (Outbox(0)'s answer, oldest first).
    void setRows(const KonedriveOutboxList &rows);

    /// A row's state and kind in a few words: "Uploading", "Waiting: open in
    /// another program", "Held until you decide"…
    static QString stateText(const QString &state, const QString &kind);

Q_SIGNALS:
    void changed();

private:
    QList<KonedriveOutboxRow> m_rows;
    int m_total = 0;
    int m_held = 0;
};
