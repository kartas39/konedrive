// The overlay plugin's logic: which emblem a file gets, a cache of which
// directories lie inside a sync root, and live updates when a file's state
// changes.
//
// Live updates come from inotify. A change to a child's extended attribute is
// reported as IN_ATTRIB on a watch of its directory, and a change to the
// directory's own attributes (its root mark, or its pin) as IN_ATTRIB with no
// name; so one watch per directory Dolphin has asked about keeps both the
// emblems and the cached root answer current. inotify reports without
// opening anything.
//
// A pin is read fresh on every call to overlays() -- it is never cached the
// way the root answer is -- so it is always correct even for an ancestor
// directory nothing here watches. Only the *live update* (overlaysChanged
// without Dolphin asking again) needs a watch: pinning or unpinning a
// directory Dolphin has browsed (so it is cached here, as the directory
// itself or as a root) reaches every file cached under it; a pin set on an
// ancestor Dolphin has only passed through, never watched on its own, is
// picked up the next time Dolphin asks (docs/limitations-and-workarounds.md).
//
// The cache holds an answer only while a watch backs it: a directory's entry
// and its watch are created, evicted and dropped together, and the sync root
// an entry names is watched too, so a root that loses its mark, moves or is
// deleted takes every answer that named it with it. At most `directoryLimit`
// directories are kept, least recently asked about evicted first.

#pragma once

#include "filestate.h"

#include <QHash>
#include <QObject>
#include <QUrl>

#include <map>

class QSocketNotifier;

namespace konedrive
{

class OverlayEngine : public QObject
{
    Q_OBJECT

public:
    static constexpr int DefaultDirectoryLimit = 256;

    explicit OverlayEngine(QObject *parent = nullptr, int directoryLimit = DefaultDirectoryLimit, RootMarkReader hasMark = hasRootMark, PinMarkReader hasPin = hasPinMark);
    ~OverlayEngine() override;

    /// The overlay icon names for `url`. Called on Dolphin's UI thread for
    /// every file it shows; answers from lstat(2) and lgetxattr(2) on the
    /// file, plus getxattr(2) on its ancestors the first time a directory is
    /// seen.
    QStringList overlays(const QUrl &url);

    /// Directories with a cached answer.
    int directoryCount() const;
    /// inotify watches currently held.
    int watchCount() const;

Q_SIGNALS:
    void overlaysChanged(const QUrl &url, const QStringList &overlays);

private:
    struct Directory {
        int watch = -1;
        /// The sync root this directory lies in, if any.
        std::optional<QString> root;
        /// The files in it Dolphin has asked about, with the emblem it was
        /// given; only kept for a directory inside a root.
        QHash<QString, Emblem> files;
        quint64 lastUse = 0;
    };

    using Changes = QList<std::pair<QUrl, QStringList>>;

    // The paths below are taken by value on purpose: a caller may hold them
    // in the very map node these functions erase.
    Directory *directoryFor(QString dir);
    void use(Directory &directory);
    bool ensureInotify();
    void makeRoom();
    void forget(QString dir);
    void forgetTree(QString dir);
    void resolveAgain(const QStringList &dirs, Changes &changes);
    void recheck(const QString &dir, const QString &name, Changes &changes);
    void readEvents();

    const int m_directoryLimit;
    const RootMarkReader m_hasMark;
    const PinMarkReader m_hasPin;
    int m_inotify = -1;
    bool m_inotifyUnavailable = false;
    bool m_warnedOutOfWatches = false;
    QSocketNotifier *m_notifier = nullptr;
    quint64 m_clock = 0;
    /// Keyed by the directory path as Dolphin spelled it; ordered, so that a
    /// directory's subtree is one range.
    std::map<QString, Directory> m_directories;
    /// A watch is per inode: two spellings of one directory (through a
    /// symbolic link) share it.
    QHash<int, QStringList> m_pathsByWatch;
};

} // namespace konedrive
