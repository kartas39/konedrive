#pragma once

#include <QDBusArgument>
#include <QList>
#include <QMetaType>
#include <QString>

/// One entry of UploadQueue.NotUploaded() and NotUploadedFiles(): (full path, reason).
struct KonedriveSkippedItem {
    QString path;
    QString reason;
};
using KonedriveSkippedList = QList<KonedriveSkippedItem>;
Q_DECLARE_METATYPE(KonedriveSkippedItem)

/// One entry of org.konedrive.Folder.Skipped(): (full path in OneDrive, reason, what keeps
/// it on this computer, where it is here). `waits` and `here` are empty for an item that is
/// not here.
struct KonedriveNotInFolderItem {
    QString path;
    QString reason;
    QString waits;
    QString here;
};
using KonedriveNotInFolderList = QList<KonedriveNotInFolderItem>;
Q_DECLARE_METATYPE(KonedriveNotInFolderItem)

inline QDBusArgument &operator<<(QDBusArgument &argument, const KonedriveNotInFolderItem &item)
{
    argument.beginStructure();
    argument << item.path << item.reason << item.waits << item.here;
    argument.endStructure();
    return argument;
}

inline const QDBusArgument &operator>>(const QDBusArgument &argument, KonedriveNotInFolderItem &item)
{
    argument.beginStructure();
    argument >> item.path >> item.reason >> item.waits >> item.here;
    argument.endStructure();
    return argument;
}

inline QDBusArgument &operator<<(QDBusArgument &argument, const KonedriveSkippedItem &item)
{
    argument.beginStructure();
    argument << item.path << item.reason;
    argument.endStructure();
    return argument;
}

inline const QDBusArgument &operator>>(const QDBusArgument &argument, KonedriveSkippedItem &item)
{
    argument.beginStructure();
    argument >> item.path >> item.reason;
    argument.endStructure();
    return argument;
}
