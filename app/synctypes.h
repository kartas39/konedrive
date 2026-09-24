#pragma once

// The structured types of org.konedrive.Sync1 (dbus/org.konedrive.Sync1.xml
// names them in its QtTypeName annotations). qdbusxml2cpp takes one include,
// so this header carries them all.

#include "skippeditem.h"

#include <QDBusArgument>
#include <QDBusMetaType>
#include <QList>
#include <QMetaType>
#include <QString>

/// One entry of RecentActivity() and ActivityAdded: (unix time, kind, full path, detail).
struct KonedriveActivity {
    qint64 time = 0;
    QString kind;
    QString path;
    QString detail;
};
using KonedriveActivityList = QList<KonedriveActivity>;
Q_DECLARE_METATYPE(KonedriveActivity)

/// The same event: every field equal.
inline bool operator==(const KonedriveActivity &a, const KonedriveActivity &b)
{
    return a.time == b.time && a.kind == b.kind && a.path == b.path && a.detail == b.detail;
}

inline QDBusArgument &operator<<(QDBusArgument &argument, const KonedriveActivity &event)
{
    argument.beginStructure();
    argument << event.time << event.kind << event.path << event.detail;
    argument.endStructure();
    return argument;
}

inline const QDBusArgument &operator>>(const QDBusArgument &argument, KonedriveActivity &event)
{
    argument.beginStructure();
    argument >> event.time >> event.kind >> event.path >> event.detail;
    argument.endStructure();
    return argument;
}

/// One entry of Conflicts(): (unix time, original full path, full path it was moved to).
struct KonedriveConflict {
    qint64 time = 0;
    QString original;
    QString rescued;
};
using KonedriveConflictList = QList<KonedriveConflict>;
Q_DECLARE_METATYPE(KonedriveConflict)

inline QDBusArgument &operator<<(QDBusArgument &argument, const KonedriveConflict &conflict)
{
    argument.beginStructure();
    argument << conflict.time << conflict.original << conflict.rescued;
    argument.endStructure();
    return argument;
}

inline const QDBusArgument &operator>>(const QDBusArgument &argument, KonedriveConflict &conflict)
{
    argument.beginStructure();
    argument >> conflict.time >> conflict.original >> conflict.rescued;
    argument.endStructure();
    return argument;
}

/// One entry of the Transfers property: (full path, bytes done, bytes total).
struct KonedriveTransfer {
    QString path;
    qulonglong done = 0;
    qulonglong total = 0;
};
using KonedriveTransferList = QList<KonedriveTransfer>;
Q_DECLARE_METATYPE(KonedriveTransfer)

inline QDBusArgument &operator<<(QDBusArgument &argument, const KonedriveTransfer &transfer)
{
    argument.beginStructure();
    argument << transfer.path << transfer.done << transfer.total;
    argument.endStructure();
    return argument;
}

inline const QDBusArgument &operator>>(const QDBusArgument &argument, KonedriveTransfer &transfer)
{
    argument.beginStructure();
    argument >> transfer.path >> transfer.done >> transfer.total;
    argument.endStructure();
    return argument;
}

/// Registers every Sync1 type with QtDBus; safe to call more than once.
inline void registerKonedriveSyncTypes()
{
    qDBusRegisterMetaType<KonedriveSkippedItem>();
    qDBusRegisterMetaType<KonedriveSkippedList>();
    qDBusRegisterMetaType<KonedriveActivity>();
    qDBusRegisterMetaType<KonedriveActivityList>();
    qDBusRegisterMetaType<KonedriveConflict>();
    qDBusRegisterMetaType<KonedriveConflictList>();
    qDBusRegisterMetaType<KonedriveTransfer>();
    qDBusRegisterMetaType<KonedriveTransferList>();
}
