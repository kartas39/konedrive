#pragma once

#include <QDBusArgument>
#include <QList>
#include <QMetaType>
#include <QString>

/// One entry of org.konedrive.Sync1.Skipped(): (full path, reason).
struct KonedriveSkippedItem {
    QString path;
    QString reason;
};
using KonedriveSkippedList = QList<KonedriveSkippedItem>;
Q_DECLARE_METATYPE(KonedriveSkippedItem)

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
