#pragma once

#include <QAbstractListModel>
#include <QDBusConnection>
#include <QHash>
#include <QList>
#include <QString>
#include <QStringList>

#include <optional>

/// The picker of "Folders on this computer": the folders of an account's
/// OneDrive as a tree, read level by level as branches open
/// (Folder.FolderChildren), each with a check mark, and the selection being
/// edited. Nothing reaches the daemon until apply(). The model's rows are the
/// folders in sight, in order, each with its depth.
///
/// A folder is checked when it is chosen or lies in a chosen folder, partly
/// checked when a chosen folder lies below it. A chosen folder's place in the
/// tree is known by its path in OneDrive (SelectedFolders), so a folder whose
/// branch was never opened still counts.
class FolderPicker : public QAbstractListModel
{
    Q_OBJECT
    /// "Sync everything": no selection. While on, every folder is checked.
    Q_PROPERTY(bool everything READ everything WRITE setEverything NOTIFY changed)
    /// "Files in the root".
    Q_PROPERTY(bool rootFiles READ rootFiles WRITE setRootFiles NOTIFY changed)
    /// The selection and the root's folders are being read.
    Q_PROPERTY(bool loading READ loading NOTIFY changed)
    /// The selection and the root's folders were read: only then can anything
    /// be changed or applied. A failed read leaves it off, with `problem` saying why.
    Q_PROPERTY(bool ready READ ready NOTIFY changed)
    /// apply() waits for the daemon's answer.
    Q_PROPERTY(bool applying READ applying NOTIFY changed)
    /// The first listing of OneDrive still runs: folders may be missing.
    Q_PROPERTY(bool incomplete READ incomplete NOTIFY changed)
    /// What is edited differs from what the daemon has.
    Q_PROPERTY(bool modified READ modified NOTIFY changed)
    /// What applying removes from this computer, in words; empty when nothing.
    Q_PROPERTY(QString summary READ summary NOTIFY changed)
    /// Why the last apply() or read was refused; LocalChanges' message lists
    /// the paths and why. Empty when it was not.
    Q_PROPERTY(QString problem READ problem NOTIFY changed)

public:
    enum Roles {
        IdRole = Qt::UserRole + 1,
        NameRole,
        DepthRole,
        /// It has sub-folders.
        ExpandableRole,
        ExpandedRole,
        /// Qt::Unchecked, Qt::PartiallyChecked or Qt::Checked.
        CheckRole,
        /// Its sub-folders are being read.
        LoadingRole,
    };
    Q_ENUM(Roles)

    FolderPicker(const QDBusConnection &bus, const QString &path, QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;
    QHash<int, QByteArray> roleNames() const override;

    bool everything() const { return m_now.everything; }
    void setEverything(bool on);
    bool rootFiles() const { return m_now.everything || m_now.rootFiles; }
    void setRootFiles(bool on);
    bool loading() const { return m_loading; }
    bool ready() const { return !m_loading && m_nodes.value(QString()).loaded; }
    bool applying() const { return m_applying; }
    bool incomplete() const { return m_incomplete; }
    bool modified() const;
    QString summary() const;
    QString problem() const { return m_problem; }

    /// The chosen folders' ids as apply() sends them.
    QStringList chosenIds() const;

    /// Starts over: reads the selection (the folder's properties) and the
    /// root's folders. What was edited and not applied is dropped.
    Q_INVOKABLE void open();
    /// Opens or closes the branch at `row`; a branch opened for the first
    /// time is read from the daemon.
    Q_INVOKABLE void toggle(int row);
    /// Checks the folder at `row` (it becomes chosen, and the chosen folders
    /// below it go), or unchecks it: a chosen folder is no longer chosen; a
    /// folder inside a chosen one makes that one give way to its other
    /// sub-folders; a partly checked folder loses the chosen folders below it.
    Q_INVOKABLE void setChecked(int row, bool checked);
    /// A click on the folder at `row`: an unchecked folder is checked; a checked or
    /// partly checked one is unchecked (a partly checked one loses every chosen
    /// folder below it, as `konedrivectl sync select remove` does).
    Q_INVOKABLE void click(int row);
    /// SyncEverything, or SetSelection with the chosen folders. applied() on
    /// success; a refusal lands in `problem` and changes nothing.
    Q_INVOKABLE void apply();

Q_SIGNALS:
    void changed();
    void applied();

private:
    struct Node {
        QString id;
        QString name;
        /// In OneDrive, relative to the root; empty for the root.
        QString path;
        QString parent;
        int depth = 0;
        bool expandable = false;
        bool expanded = false;
        bool loaded = false;
        bool loading = false;
        QStringList children;
    };
    /// A chosen folder: its id, and its path when the daemon's list holds it.
    struct Chosen {
        QString id;
        QString path;
    };
    struct Selection {
        bool everything = true;
        bool rootFiles = true;
        QList<Chosen> chosen;
    };

    void load(const QString &id, int generation);
    void insertChildren(const QString &id);
    /// The chosen folder's path now: its node's, when its branch was read.
    QString pathOf(const Chosen &chosen) const;
    Qt::CheckState stateOf(const QString &id, const QString &path, const Selection &selection) const;
    Qt::CheckState stateOf(const Node &node, const Selection &selection) const;
    void removeBelow(const QString &path);
    void touched();
    void fail(const QString &message);

    QDBusConnection m_bus;
    QString m_path;
    /// By id; the root is "".
    QHash<QString, Node> m_nodes;
    /// The folders in sight, by id.
    QStringList m_rows;
    Selection m_now;
    Selection m_initial;
    /// The list that "Sync everything" switched away from, to come back to.
    std::optional<Selection> m_saved;
    bool m_loading = false;
    bool m_applying = false;
    bool m_incomplete = false;
    QString m_problem;
    /// open() calls so far: an answer to an earlier one is dropped.
    int m_generation = 0;
};
