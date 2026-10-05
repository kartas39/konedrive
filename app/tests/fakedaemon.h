#pragma once

// A stand-in for konedrived on the tests' private session bus, shaped as the
// daemon serves it: the manager at /org/konedrive/Accounts with Accounts
// (dbus/org.konedrive.Accounts.xml), and each account at
// /org/konedrive/Accounts/<id> with Account and its folder's Folder, Transfers,
// UploadQueue, Conflicts, LocalScan and ActivityLog. Each interface is an
// adaptor, since one D-Bus path holds one object. What the controllers ask is
// logged in `calls`; properties change through set(), which also emits
// PropertiesChanged the way the daemon does.

#include "accountcontroller.h"
#include "daemoncontroller.h"
#include "synccontroller.h"
#include "synctypes.h"

#include <QDBusAbstractAdaptor>
#include <QDBusConnection>
#include <QDBusMessage>
#include <QDBusObjectPath>
#include <QHash>
#include <QStringList>
#include <QTimer>
#include <QVariantMap>

#include <memory>

namespace fake
{
inline const QString BusName = QStringLiteral("fake-daemon");
inline const QString ManagerPath = QStringLiteral("/org/konedrive/Accounts");

/// The n-th account's id (from 1): 12 hex characters, as the daemon's are.
inline QString idFor(int n)
{
    return QStringLiteral("%1").arg(n, 12, 16, QLatin1Char('0'));
}

inline QString accountPath(const QString &id)
{
    return ManagerPath + QLatin1Char('/') + id;
}

/// The path of FakeDaemon's first account.
inline const QString FirstAccount = accountPath(idFor(1));

/// The fake's own connection, so the controllers under test (on the
/// default session connection) talk to it over the bus.
inline QDBusConnection bus()
{
    return QDBusConnection::connectToBus(QDBusConnection::SessionBus, BusName);
}

inline void propertiesChanged(QDBusConnection connection, const QString &path, const QString &interfaceName, const QVariantMap &changes)
{
    auto signal = QDBusMessage::createSignal(path, QStringLiteral("org.freedesktop.DBus.Properties"), QStringLiteral("PropertiesChanged"));
    signal << interfaceName << changes << QStringList();
    connection.send(signal);
}
}

class FakeAccount : public QDBusAbstractAdaptor
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.konedrive.Account")
    Q_PROPERTY(QString Id READ id)
    Q_PROPERTY(QString Label READ label)
    Q_PROPERTY(QString Mode READ mode)
    Q_PROPERTY(QString State READ state)
    Q_PROPERTY(QString LastError READ lastError)
    Q_PROPERTY(QString DisplayName READ displayName)
    Q_PROPERTY(QString Email READ email)
    Q_PROPERTY(qulonglong QuotaUsed READ quotaUsed)
    Q_PROPERTY(qulonglong QuotaTotal READ quotaTotal)
    Q_PROPERTY(qulonglong QuotaRemaining READ quotaRemaining)
    Q_PROPERTY(QString QuotaState READ quotaState)

public:
    FakeAccount(QObject *parent, const QDBusConnection &bus, const QString &path, const QString &id, const QString &label)
        : QDBusAbstractAdaptor(parent)
        , m_bus(bus)
        , m_path(path)
    {
        m_properties.insert(QStringLiteral("Id"), id);
        m_properties.insert(QStringLiteral("Label"), label);
    }

    QString id() const { return m_properties.value(QStringLiteral("Id")).toString(); }
    QString label() const { return m_properties.value(QStringLiteral("Label")).toString(); }
    QString mode() const { return m_properties.value(QStringLiteral("Mode")).toString(); }
    QString state() const { return m_properties.value(QStringLiteral("State")).toString(); }
    QString lastError() const { return m_properties.value(QStringLiteral("LastError")).toString(); }
    QString displayName() const { return m_properties.value(QStringLiteral("DisplayName")).toString(); }
    QString email() const { return m_properties.value(QStringLiteral("Email")).toString(); }
    qulonglong quotaUsed() const { return m_properties.value(QStringLiteral("QuotaUsed")).toULongLong(); }
    qulonglong quotaTotal() const { return m_properties.value(QStringLiteral("QuotaTotal")).toULongLong(); }
    qulonglong quotaRemaining() const { return m_properties.value(QStringLiteral("QuotaRemaining")).toULongLong(); }
    QString quotaState() const { return m_properties.value(QStringLiteral("QuotaState")).toString(); }

    void set(const QVariantMap &changes)
    {
        for (auto it = changes.cbegin(); it != changes.cend(); ++it) {
            m_properties.insert(it.key(), it.value());
        }
        fake::propertiesChanged(m_bus, m_path, AccountController::InterfaceName, changes);
    }

    /// The sign-in a switch to read-write waits for, granted.
    void grantReadWrite() { set({{QStringLiteral("Mode"), QStringLiteral("read-write")}}); }

    QStringList calls;
    /// SetMode("read-write") gets through the development gate, which refuses by default.
    bool gateOpen = false;
    /// Changes waiting to be uploaded, for SetMode("read-only").
    uint pendingUploads = 0;

public Q_SLOTS:
    QString BeginSignIn()
    {
        calls << QStringLiteral("BeginSignIn");
        set({{QStringLiteral("State"), QStringLiteral("signing-in")}});
        return QStringLiteral("https://login.example/authorize?account=") + id();
    }
    void CancelSignIn()
    {
        calls << QStringLiteral("CancelSignIn");
        // A switch to read-write's sign-in is given up with the account still signed in.
        if (state() == QLatin1String("signing-in")) {
            set({{QStringLiteral("State"), QStringLiteral("signed-out")}});
        }
    }
    void SignOut()
    {
        calls << QStringLiteral("SignOut");
        set({{QStringLiteral("State"), QStringLiteral("signed-out")}});
    }
    void RefreshInfo() { calls << QStringLiteral("RefreshInfo"); }
    void SetLabel(const QString &label, const QDBusMessage &message)
    {
        calls << QStringLiteral("SetLabel:") + label;
        if (label.trimmed().isEmpty() || label.contains(QLatin1Char('/'))) {
            message.setDelayedReply(true);
            m_bus.send(message.createErrorReply(QStringLiteral("org.freedesktop.DBus.Error.InvalidArgs"), QStringLiteral("not a label: ") + label));
            return;
        }
        set({{QStringLiteral("Label"), label.trimmed()}});
    }
    /// As the daemon's: the gate first, then the sign-in state, for read-write, which
    /// answers a sign-in URL (grantReadWrite() then ends it); PendingUploads for
    /// read-only while `pendingUploads` is not 0, unless forced, which drops them.
    QString SetMode(const QString &mode, bool force, const QDBusMessage &message)
    {
        calls << QStringLiteral("SetMode:%1:%2").arg(mode, force ? QStringLiteral("force") : QStringLiteral("no-force"));
        const auto refuse = [&](const QString &name, const QString &why) {
            message.setDelayedReply(true);
            m_bus.send(message.createErrorReply(name, why));
            return QString();
        };
        if (mode == QLatin1String("read-write")) {
            if (!gateOpen) {
                return refuse(QStringLiteral("org.konedrive.Error.WritesNotAllowed"), QStringLiteral("DAEMON-GATE-WORDS"));
            }
            if (state() != QLatin1String("signed-in")) {
                return refuse(QStringLiteral("org.konedrive.Error.NotSignedIn"), QStringLiteral("sign in first; then switch the account to read-write"));
            }
            set({{QStringLiteral("LastError"), QString()}});
            return QStringLiteral("https://login.example/authorize?scope=Files.ReadWrite&account=") + id();
        }
        if (mode == QLatin1String("read-only")) {
            if (pendingUploads > 0 && !force) {
                return refuse(QStringLiteral("org.konedrive.Error.PendingUploads"),
                              QStringLiteral("%1 changes made here have not been uploaded yet").arg(pendingUploads));
            }
            pendingUploads = 0;
            set({{QStringLiteral("Mode"), QStringLiteral("read-only")}});
            return QString();
        }
        return refuse(QStringLiteral("org.freedesktop.DBus.Error.InvalidArgs"), QStringLiteral("unknown mode ") + mode);
    }

private:
    QDBusConnection m_bus;
    QString m_path;
    QVariantMap m_properties{
        {QStringLiteral("Mode"), QStringLiteral("read-only")},
        {QStringLiteral("State"), QStringLiteral("signed-out")},
        {QStringLiteral("LastError"), QString()},
        {QStringLiteral("DisplayName"), QString()},
        {QStringLiteral("Email"), QString()},
        {QStringLiteral("QuotaUsed"), QVariant::fromValue<qulonglong>(0)},
        {QStringLiteral("QuotaTotal"), QVariant::fromValue<qulonglong>(0)},
        {QStringLiteral("QuotaRemaining"), QVariant::fromValue<qulonglong>(0)},
        {QStringLiteral("QuotaState"), QString()},
    };
};

class FakeSync;

/// One interface of an account's folder: its properties, changed through set(),
/// which also emits PropertiesChanged under that interface, as the daemon does.
class FakeFolderInterface : public QDBusAbstractAdaptor
{
public:
    FakeFolderInterface(QObject *parent, FakeSync *sync, const QString &interfaceName, const QVariantMap &properties);

    void set(const QVariantMap &changes);
    QVariant value(const char *key) const { return m_properties.value(QLatin1String(key)); }

protected:
    FakeSync *m_sync;
    QString m_interface;
    QVariantMap m_properties;
};

class FakeFolder : public FakeFolderInterface
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.konedrive.Folder")
    Q_PROPERTY(QString Path READ path)
    Q_PROPERTY(QString State READ state)
    Q_PROPERTY(QString Source READ source)
    Q_PROPERTY(QString LastError READ lastError)
    Q_PROPERTY(qulonglong ItemsListed READ itemsListed)
    Q_PROPERTY(qulonglong ItemsPlaced READ itemsPlaced)
    Q_PROPERTY(qulonglong SkippedCount READ skippedCount)
    Q_PROPERTY(qlonglong LastChecked READ lastChecked)
    Q_PROPERTY(qulonglong LocalBytes READ localBytes)
    Q_PROPERTY(uint PinnedCount READ pinnedCount)
    Q_PROPERTY(QStringList IgnorePatterns READ ignorePatterns)
    Q_PROPERTY(bool Paused READ paused)
    Q_PROPERTY(qlonglong PausedUntil READ pausedUntil)
    Q_PROPERTY(QString HeldBack READ heldBack)
    Q_PROPERTY(bool Writable READ writable)
    Q_PROPERTY(QString LiveChanges READ liveChanges)
    Q_PROPERTY(bool Thumbnails READ thumbnails)

public:
    FakeFolder(QObject *parent, FakeSync *sync)
        : FakeFolderInterface(parent,
                              sync,
                              SyncController::FolderInterface,
                              {
                                  {QStringLiteral("Path"), QString()},
                                  {QStringLiteral("State"), QStringLiteral("none")},
                                  {QStringLiteral("Source"), QString()},
                                  {QStringLiteral("LastError"), QString()},
                                  {QStringLiteral("ItemsListed"), QVariant::fromValue<qulonglong>(0)},
                                  {QStringLiteral("ItemsPlaced"), QVariant::fromValue<qulonglong>(0)},
                                  {QStringLiteral("SkippedCount"), QVariant::fromValue<qulonglong>(0)},
                                  {QStringLiteral("LastChecked"), QVariant::fromValue<qlonglong>(0)},
                                  {QStringLiteral("LocalBytes"), QVariant::fromValue<qulonglong>(0)},
                                  {QStringLiteral("PinnedCount"), QVariant::fromValue<uint>(0)},
                                  {QStringLiteral("IgnorePatterns"), QStringList{QStringLiteral("*.tmp"), QStringLiteral("~*")}},
                                  {QStringLiteral("Paused"), false},
                                  {QStringLiteral("PausedUntil"), QVariant::fromValue<qlonglong>(0)},
                                  {QStringLiteral("HeldBack"), QString()},
                                  {QStringLiteral("Writable"), true},
                                  {QStringLiteral("LiveChanges"), QStringLiteral("off")},
                                  {QStringLiteral("Thumbnails"), true},
                              })
    {
    }

    QString path() const { return value("Path").toString(); }
    QString state() const { return value("State").toString(); }
    QString source() const { return value("Source").toString(); }
    QString lastError() const { return value("LastError").toString(); }
    qulonglong itemsListed() const { return value("ItemsListed").toULongLong(); }
    qulonglong itemsPlaced() const { return value("ItemsPlaced").toULongLong(); }
    qulonglong skippedCount() const { return value("SkippedCount").toULongLong(); }
    qlonglong lastChecked() const { return value("LastChecked").toLongLong(); }
    qulonglong localBytes() const { return value("LocalBytes").toULongLong(); }
    uint pinnedCount() const { return value("PinnedCount").toUInt(); }
    QStringList ignorePatterns() const { return value("IgnorePatterns").toStringList(); }
    bool paused() const { return value("Paused").toBool(); }
    qlonglong pausedUntil() const { return value("PausedUntil").toLongLong(); }
    QString heldBack() const { return value("HeldBack").toString(); }
    bool writable() const { return value("Writable").toBool(); }
    QString liveChanges() const { return value("LiveChanges").toString(); }
    bool thumbnails() const { return value("Thumbnails").toBool(); }

public Q_SLOTS:
    void Register(const QString &path, const QDBusMessage &message);
    void RegisterWithoutInterception(const QString &path);
    void Unregister();
    void Refresh(const QDBusMessage &message);
    KonedriveNotInFolderList Skipped();
    /// Answers (u files, t bytes, u busy) by hand, so that it can be held.
    void FreeUpSpace(const QDBusMessage &message);
    void Pause(uint seconds);
    void Resume();
    void SetIgnorePatterns(const QStringList &patterns, const QDBusMessage &message);
    void SyncAnyway();
    void SetThumbnails(bool on);
};

class FakeTransfers : public FakeFolderInterface
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.konedrive.Transfers")
    Q_PROPERTY(KonedriveTransferList Downloads READ downloads)
    Q_PROPERTY(KonedriveTransferList Uploads READ uploads)
    Q_PROPERTY(qulonglong DownloadSpeed READ downloadSpeed)
    Q_PROPERTY(qulonglong UploadSpeed READ uploadSpeed)
    Q_PROPERTY(uint ActiveDownloads READ activeDownloads)
    Q_PROPERTY(uint ActiveUploads READ activeUploads)
    Q_PROPERTY(uint PoolInUse READ poolInUse)
    Q_PROPERTY(uint PoolSize READ poolSize)
    Q_PROPERTY(uint PoolCeiling READ poolCeiling)
    Q_PROPERTY(uint LargeFiles READ largeFiles)
    Q_PROPERTY(uint LargeStreams READ largeStreams)
    Q_PROPERTY(uint LargeStreamLimit READ largeStreamLimit)
    Q_PROPERTY(uint RetryAfter READ retryAfter)
    Q_PROPERTY(uint DownloadLeftCount READ downloadLeftCount)
    Q_PROPERTY(qulonglong DownloadLeftBytes READ downloadLeftBytes)
    Q_PROPERTY(qulonglong DownloadDoneBytes READ downloadDoneBytes)
    Q_PROPERTY(uint DownloadTimeLeft READ downloadTimeLeft)
    Q_PROPERTY(uint UploadLeftCount READ uploadLeftCount)
    Q_PROPERTY(qulonglong UploadLeftBytes READ uploadLeftBytes)
    Q_PROPERTY(qulonglong UploadDoneBytes READ uploadDoneBytes)
    Q_PROPERTY(uint UploadTimeLeft READ uploadTimeLeft)

public:
    FakeTransfers(QObject *parent, FakeSync *sync)
        : FakeFolderInterface(parent,
                              sync,
                              SyncController::TransfersInterface,
                              {
                                  {QStringLiteral("DownloadSpeed"), QVariant::fromValue<qulonglong>(0)},
                                  {QStringLiteral("UploadSpeed"), QVariant::fromValue<qulonglong>(0)},
                                  {QStringLiteral("ActiveDownloads"), QVariant::fromValue<uint>(0)},
                                  {QStringLiteral("ActiveUploads"), QVariant::fromValue<uint>(0)},
                                  {QStringLiteral("PoolInUse"), QVariant::fromValue<uint>(0)},
                                  {QStringLiteral("PoolSize"), QVariant::fromValue<uint>(16)},
                                  {QStringLiteral("PoolCeiling"), QVariant::fromValue<uint>(64)},
                                  {QStringLiteral("LargeFiles"), QVariant::fromValue<uint>(0)},
                                  {QStringLiteral("LargeStreams"), QVariant::fromValue<uint>(0)},
                                  {QStringLiteral("LargeStreamLimit"), QVariant::fromValue<uint>(4)},
                                  {QStringLiteral("RetryAfter"), QVariant::fromValue<uint>(0)},
                                  {QStringLiteral("DownloadLeftCount"), QVariant::fromValue<uint>(0)},
                                  {QStringLiteral("DownloadLeftBytes"), QVariant::fromValue<qulonglong>(0)},
                                  {QStringLiteral("DownloadDoneBytes"), QVariant::fromValue<qulonglong>(0)},
                                  {QStringLiteral("DownloadTimeLeft"), QVariant::fromValue<uint>(0)},
                                  {QStringLiteral("UploadLeftCount"), QVariant::fromValue<uint>(0)},
                                  {QStringLiteral("UploadLeftBytes"), QVariant::fromValue<qulonglong>(0)},
                                  {QStringLiteral("UploadDoneBytes"), QVariant::fromValue<qulonglong>(0)},
                                  {QStringLiteral("UploadTimeLeft"), QVariant::fromValue<uint>(0)},
                              })
    {
    }

    KonedriveTransferList downloads() const { return m_downloads; }
    KonedriveTransferList uploads() const { return m_uploads; }
    qulonglong downloadSpeed() const { return value("DownloadSpeed").toULongLong(); }
    qulonglong uploadSpeed() const { return value("UploadSpeed").toULongLong(); }
    uint activeDownloads() const { return value("ActiveDownloads").toUInt(); }
    uint activeUploads() const { return value("ActiveUploads").toUInt(); }
    uint poolInUse() const { return value("PoolInUse").toUInt(); }
    uint poolSize() const { return value("PoolSize").toUInt(); }
    uint poolCeiling() const { return value("PoolCeiling").toUInt(); }
    uint largeFiles() const { return value("LargeFiles").toUInt(); }
    uint largeStreams() const { return value("LargeStreams").toUInt(); }
    uint largeStreamLimit() const { return value("LargeStreamLimit").toUInt(); }
    uint retryAfter() const { return value("RetryAfter").toUInt(); }
    uint downloadLeftCount() const { return value("DownloadLeftCount").toUInt(); }
    qulonglong downloadLeftBytes() const { return value("DownloadLeftBytes").toULongLong(); }
    qulonglong downloadDoneBytes() const { return value("DownloadDoneBytes").toULongLong(); }
    uint downloadTimeLeft() const { return value("DownloadTimeLeft").toUInt(); }
    uint uploadLeftCount() const { return value("UploadLeftCount").toUInt(); }
    qulonglong uploadLeftBytes() const { return value("UploadLeftBytes").toULongLong(); }
    qulonglong uploadDoneBytes() const { return value("UploadDoneBytes").toULongLong(); }
    uint uploadTimeLeft() const { return value("UploadTimeLeft").toUInt(); }

    void setDownloads(const KonedriveTransferList &downloads)
    {
        m_downloads = downloads;
        set({{QStringLiteral("Downloads"), QVariant::fromValue(downloads)}});
    }

    void setUploads(const KonedriveTransferList &uploads)
    {
        m_uploads = uploads;
        set({{QStringLiteral("Uploads"), QVariant::fromValue(uploads)}});
    }

private:
    KonedriveTransferList m_downloads;
    KonedriveTransferList m_uploads;
};

class FakeUploadQueue : public FakeFolderInterface
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.konedrive.UploadQueue")
    Q_PROPERTY(uint PendingCount READ pendingCount)
    Q_PROPERTY(qulonglong PendingBytes READ pendingBytes)
    Q_PROPERTY(uint BlockedCount READ blockedCount)
    Q_PROPERTY(uint HeldCount READ heldCount)
    Q_PROPERTY(bool QuotaFull READ quotaFull)
    Q_PROPERTY(uint QuotaWaitingCount READ quotaWaitingCount)
    Q_PROPERTY(qulonglong QuotaWaitingBytes READ quotaWaitingBytes)
    Q_PROPERTY(uint TooBigCount READ tooBigCount)

public:
    FakeUploadQueue(QObject *parent, FakeSync *sync)
        : FakeFolderInterface(parent,
                              sync,
                              SyncController::UploadQueueInterface,
                              {
                                  {QStringLiteral("PendingCount"), QVariant::fromValue<uint>(0)},
                                  {QStringLiteral("PendingBytes"), QVariant::fromValue<qulonglong>(0)},
                                  {QStringLiteral("BlockedCount"), QVariant::fromValue<uint>(0)},
                                  {QStringLiteral("HeldCount"), QVariant::fromValue<uint>(0)},
                                  {QStringLiteral("QuotaFull"), false},
                                  {QStringLiteral("QuotaWaitingCount"), QVariant::fromValue<uint>(0)},
                                  {QStringLiteral("QuotaWaitingBytes"), QVariant::fromValue<qulonglong>(0)},
                                  {QStringLiteral("TooBigCount"), QVariant::fromValue<uint>(0)},
                              })
    {
    }

    uint pendingCount() const { return value("PendingCount").toUInt(); }
    qulonglong pendingBytes() const { return value("PendingBytes").toULongLong(); }
    uint blockedCount() const { return value("BlockedCount").toUInt(); }
    uint heldCount() const { return value("HeldCount").toUInt(); }
    bool quotaFull() const { return value("QuotaFull").toBool(); }
    uint quotaWaitingCount() const { return value("QuotaWaitingCount").toUInt(); }
    qulonglong quotaWaitingBytes() const { return value("QuotaWaitingBytes").toULongLong(); }
    uint tooBigCount() const { return value("TooBigCount").toUInt(); }

public Q_SLOTS:
    KonedriveOutboxList Changes(uint limit);
    uint ConfirmDeletes();
    uint RestoreDeletes();
    KonedriveSkippedList NotUploaded();
    KonedriveKeptBackList NotUploadedSummary();
    KonedriveSkippedList NotUploadedFiles(const QString &reason, uint limit, uint &total);
};

class FakeConflicts : public FakeFolderInterface
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.konedrive.Conflicts")
    Q_PROPERTY(uint Count READ count)
    Q_PROPERTY(QString MachineName READ machineName)

public:
    FakeConflicts(QObject *parent, FakeSync *sync)
        : FakeFolderInterface(parent,
                              sync,
                              SyncController::ConflictsInterface,
                              {{QStringLiteral("Count"), QVariant::fromValue<uint>(0)}, {QStringLiteral("MachineName"), QStringLiteral("fedora")}})
    {
    }

    uint count() const { return value("Count").toUInt(); }
    QString machineName() const { return value("MachineName").toString(); }

public Q_SLOTS:
    KonedriveConflictList List();
    /// Removes the row and answers; it does not emit Count, so a
    /// test sees whether the window asks again on its own.
    void Dismiss(const QString &rescued, const QDBusMessage &message);
};

class FakeLocalScan : public FakeFolderInterface
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.konedrive.LocalScan")
    Q_PROPERTY(QString State READ state)
    Q_PROPERTY(QString Reason READ reason)
    Q_PROPERTY(qlonglong Started READ started)
    Q_PROPERTY(qulonglong Directories READ directories)
    Q_PROPERTY(qulonglong Files READ files)
    Q_PROPERTY(qulonglong Expected READ expected)
    Q_PROPERTY(qlonglong Finished READ finished)
    Q_PROPERTY(uint Took READ took)

public:
    FakeLocalScan(QObject *parent, FakeSync *sync)
        : FakeFolderInterface(parent,
                              sync,
                              SyncController::LocalScanInterface,
                              {
                                  {QStringLiteral("State"), QStringLiteral("none")},
                                  {QStringLiteral("Reason"), QString()},
                                  {QStringLiteral("Started"), QVariant::fromValue<qlonglong>(0)},
                                  {QStringLiteral("Directories"), QVariant::fromValue<qulonglong>(0)},
                                  {QStringLiteral("Files"), QVariant::fromValue<qulonglong>(0)},
                                  {QStringLiteral("Expected"), QVariant::fromValue<qulonglong>(0)},
                                  {QStringLiteral("Finished"), QVariant::fromValue<qlonglong>(0)},
                                  {QStringLiteral("Took"), QVariant::fromValue<uint>(0)},
                              })
    {
    }

    QString state() const { return value("State").toString(); }
    QString reason() const { return value("Reason").toString(); }
    qlonglong started() const { return value("Started").toLongLong(); }
    qulonglong directories() const { return value("Directories").toULongLong(); }
    qulonglong files() const { return value("Files").toULongLong(); }
    qulonglong expected() const { return value("Expected").toULongLong(); }
    qlonglong finished() const { return value("Finished").toLongLong(); }
    uint took() const { return value("Took").toUInt(); }
};

class FakeActivityLog : public QDBusAbstractAdaptor
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.konedrive.ActivityLog")

public:
    FakeActivityLog(QObject *parent, FakeSync *sync)
        : QDBusAbstractAdaptor(parent)
        , m_sync(sync)
    {
    }

public Q_SLOTS:
    KonedriveActivityList Recent(uint limit, const QDBusMessage &message);

private:
    FakeSync *m_sync;
};

/// One account's folder, shaped as the daemon serves it: an adaptor per interface
/// (`folder`, `transfers`, `queue`, `conflicts`, `scan`, `activityLog`) on the account's
/// object, and what they share. What the controllers ask is logged in `calls`.
class FakeSync
{
public:
    FakeSync(QObject *object, const QDBusConnection &bus, const QString &path)
        : bus(bus)
        , path(path)
        , folder(new FakeFolder(object, this))
        , transfers(new FakeTransfers(object, this))
        , queue(new FakeUploadQueue(object, this))
        , conflicts(new FakeConflicts(object, this))
        , scan(new FakeLocalScan(object, this))
        , activityLog(new FakeActivityLog(object, this))
    {
    }

    /// Transfers.Downloads.
    void setTransfers(const KonedriveTransferList &downloads) { transfers->setDownloads(downloads); }
    void setUploads(const KonedriveTransferList &uploads) { transfers->setUploads(uploads); }

    /// The mass-delete guard trips: `count` removals of `path`'s kind are
    /// held, in the queue and in HeldCount.
    void holdDeletes(const QString &path, uint count)
    {
        for (uint i = 0; i < count; ++i) {
            outboxRows << KonedriveOutboxRow{100 + i, QStringLiteral("delete"), path + QString::number(i), QStringLiteral("held"), 0, 0, QStringLiteral("mass-delete"), 0};
        }
        queue->set({{QStringLiteral("HeldCount"), QVariant::fromValue<uint>(queue->heldCount() + count)}});
    }

    /// Records an event in Recent() and emits ActivityLog.Added, as the daemon does.
    void activity(qint64 time, const QString &kind, const QString &path, const QString &detail)
    {
        log.prepend({time, kind, path, detail});
        signalOnly(time, kind, path, detail);
    }

    /// Emits ActivityLog.Added without storing the event: a daemon that signals
    /// before its store has it.
    void signalOnly(qint64 time, const QString &kind, const QString &path, const QString &detail)
    {
        auto signal = QDBusMessage::createSignal(this->path, SyncController::ActivityLogInterface, QStringLiteral("Added"));
        signal << time << kind << path << detail;
        bus.send(signal);
    }

    /// Answers the held Recent() call with the log as it is now.
    void releaseActivity()
    {
        bus.send(heldActivity.createReply(QVariant::fromValue(log.mid(0, int(heldLimit)))));
        heldActivity = QDBusMessage();
    }

    /// Answers the held FreeUpSpace() call.
    void finishFreeUp()
    {
        bus.send(heldFreeUp.createReply({QVariant::fromValue(freedFiles), QVariant::fromValue(freedBytes), QVariant::fromValue(busyFiles)}));
        heldFreeUp = QDBusMessage();
    }

    QDBusConnection bus;
    const QString path;
    FakeFolder *folder;
    FakeTransfers *transfers;
    FakeUploadQueue *queue;
    FakeConflicts *conflicts;
    FakeLocalScan *scan;
    FakeActivityLog *activityLog;

    QStringList calls;
    bool helperMissing = false;
    /// Hold these calls unanswered (until releaseActivity/finishFreeUp; Refresh for ever).
    bool holdActivity = false;
    bool holdFreeUp = false;
    bool holdRefresh = false;
    /// Recent(), newest first.
    KonedriveActivityList log;
    KonedriveConflictList conflictList;
    /// Skipped().
    KonedriveNotInFolderList skippedList{{QStringLiteral("/home/u/OneDrive/Personal Vault"), QStringLiteral("personal-vault"), QString(), QString()}};
    uint freedFiles = 0;
    qulonglong freedBytes = 0;
    uint busyFiles = 0;
    /// Changes(), oldest first; Confirm/RestoreDeletes act on its "held" rows
    /// and set HeldCount to 0. holdDeletes() holds some, as the guard does.
    KonedriveOutboxList outboxRows;
    /// NotUploaded().
    KonedriveSkippedList notUploadedList;
    /// NotUploadedSummary(), and NotUploadedFiles() by reason.
    KonedriveKeptBackList keptBack;
    QHash<QString, KonedriveSkippedList> keptBackFiles;
    /// Pause(seconds) ends at pauseNow + seconds.
    qint64 pauseNow = 1758700000;
    QDBusMessage heldActivity;
    uint heldLimit = 0;
    QDBusMessage heldFreeUp;
};

inline FakeFolderInterface::FakeFolderInterface(QObject *parent, FakeSync *sync, const QString &interfaceName, const QVariantMap &properties)
    : QDBusAbstractAdaptor(parent)
    , m_sync(sync)
    , m_interface(interfaceName)
    , m_properties(properties)
{
}

inline void FakeFolderInterface::set(const QVariantMap &changes)
{
    for (auto it = changes.cbegin(); it != changes.cend(); ++it) {
        m_properties.insert(it.key(), it.value());
    }
    fake::propertiesChanged(m_sync->bus, m_sync->path, m_interface, changes);
}

inline void FakeFolder::Register(const QString &path, const QDBusMessage &message)
{
    m_sync->calls << QStringLiteral("Register:") + path;
    if (m_sync->helperMissing) {
        message.setDelayedReply(true);
        m_sync->bus.send(message.createErrorReply(QStringLiteral("org.konedrive.Error.NoHelper"), QStringLiteral("the konedrive helper is not connected")));
        return;
    }
    set({{QStringLiteral("Path"), path}, {QStringLiteral("State"), QStringLiteral("listing")}, {QStringLiteral("Source"), QStringLiteral("onedrive")}});
}

inline void FakeFolder::RegisterWithoutInterception(const QString &path)
{
    m_sync->calls << QStringLiteral("RegisterWithoutInterception:") + path;
    set({{QStringLiteral("Path"), path}, {QStringLiteral("State"), QStringLiteral("no-interception")}, {QStringLiteral("Source"), QStringLiteral("onedrive")}});
}

inline void FakeFolder::Unregister()
{
    m_sync->calls << QStringLiteral("Unregister");
    set({{QStringLiteral("Path"), QString()}, {QStringLiteral("State"), QStringLiteral("none")}, {QStringLiteral("Source"), QString()}});
}

inline void FakeFolder::Refresh(const QDBusMessage &message)
{
    m_sync->calls << QStringLiteral("Refresh");
    if (m_sync->holdRefresh) {
        message.setDelayedReply(true); // never answered
    }
}

inline KonedriveNotInFolderList FakeFolder::Skipped()
{
    m_sync->calls << QStringLiteral("Skipped");
    return m_sync->skippedList;
}

inline void FakeFolder::FreeUpSpace(const QDBusMessage &message)
{
    m_sync->calls << QStringLiteral("FreeUpSpace");
    message.setDelayedReply(true);
    m_sync->heldFreeUp = message;
    if (!m_sync->holdFreeUp) {
        m_sync->finishFreeUp();
    }
}

inline void FakeFolder::Pause(uint seconds)
{
    m_sync->calls << QStringLiteral("Pause:") + QString::number(seconds);
    set({{QStringLiteral("Paused"), true}, {QStringLiteral("PausedUntil"), QVariant::fromValue<qlonglong>(seconds == 0 ? 0 : m_sync->pauseNow + seconds)}});
}

inline void FakeFolder::Resume()
{
    m_sync->calls << QStringLiteral("Resume");
    set({{QStringLiteral("Paused"), false}, {QStringLiteral("PausedUntil"), QVariant::fromValue<qlonglong>(0)}});
}

inline void FakeFolder::SetIgnorePatterns(const QStringList &patterns, const QDBusMessage &message)
{
    m_sync->calls << QStringLiteral("SetIgnorePatterns:") + patterns.join(QLatin1Char(','));
    for (const QString &pattern : patterns) {
        if (pattern.isEmpty() || pattern.contains(QLatin1Char('/'))) {
            message.setDelayedReply(true);
            m_sync->bus.send(message.createErrorReply(QStringLiteral("org.freedesktop.DBus.Error.InvalidArgs"), QStringLiteral("not a pattern: ") + pattern));
            return;
        }
    }
    set({{QStringLiteral("IgnorePatterns"), patterns}});
}

inline void FakeFolder::SyncAnyway()
{
    m_sync->calls << QStringLiteral("SyncAnyway");
    set({{QStringLiteral("HeldBack"), QString()}});
}

inline void FakeFolder::SetThumbnails(bool on)
{
    m_sync->calls << QStringLiteral("SetThumbnails:") + (on ? QStringLiteral("on") : QStringLiteral("off"));
    set({{QStringLiteral("Thumbnails"), on}});
}

inline KonedriveOutboxList FakeUploadQueue::Changes(uint limit)
{
    m_sync->calls << QStringLiteral("Changes");
    return limit == 0 ? m_sync->outboxRows : m_sync->outboxRows.mid(0, int(limit));
}

inline uint FakeUploadQueue::ConfirmDeletes()
{
    m_sync->calls << QStringLiteral("ConfirmDeletes");
    uint released = 0;
    for (KonedriveOutboxRow &row : m_sync->outboxRows) {
        if (row.state == QLatin1String("held")) {
            row.state = QStringLiteral("ready");
            row.reason.clear();
            ++released;
        }
    }
    set({{QStringLiteral("HeldCount"), QVariant::fromValue<uint>(0)}});
    return released;
}

inline uint FakeUploadQueue::RestoreDeletes()
{
    m_sync->calls << QStringLiteral("RestoreDeletes");
    const auto dropped = m_sync->outboxRows.removeIf([](const KonedriveOutboxRow &row) {
        return row.state == QLatin1String("held");
    });
    set({{QStringLiteral("HeldCount"), QVariant::fromValue<uint>(0)}});
    return uint(dropped);
}

inline KonedriveSkippedList FakeUploadQueue::NotUploaded()
{
    m_sync->calls << QStringLiteral("NotUploaded");
    return m_sync->notUploadedList;
}

inline KonedriveKeptBackList FakeUploadQueue::NotUploadedSummary()
{
    m_sync->calls << QStringLiteral("NotUploadedSummary");
    return m_sync->keptBack;
}

inline KonedriveSkippedList FakeUploadQueue::NotUploadedFiles(const QString &reason, uint limit, uint &total)
{
    m_sync->calls << QStringLiteral("NotUploadedFiles:%1:%2").arg(reason).arg(limit);
    const KonedriveSkippedList all = m_sync->keptBackFiles.value(reason);
    total = uint(all.size());
    return limit == 0 ? all : all.mid(0, int(limit));
}

inline KonedriveConflictList FakeConflicts::List()
{
    m_sync->calls << QStringLiteral("List");
    return m_sync->conflictList;
}

inline void FakeConflicts::Dismiss(const QString &rescued, const QDBusMessage &message)
{
    m_sync->calls << QStringLiteral("Dismiss:") + rescued;
    const auto before = m_sync->conflictList.size();
    m_sync->conflictList.removeIf([&rescued](const KonedriveConflict &c) {
        return c.rescued == rescued;
    });
    if (m_sync->conflictList.size() == before) {
        message.setDelayedReply(true);
        m_sync->bus.send(message.createErrorReply(QStringLiteral("org.freedesktop.DBus.Error.InvalidArgs"), QStringLiteral("no conflict at ") + rescued));
    }
}

inline KonedriveActivityList FakeActivityLog::Recent(uint limit, const QDBusMessage &message)
{
    m_sync->calls << QStringLiteral("Recent:") + QString::number(limit);
    if (m_sync->holdActivity) {
        message.setDelayedReply(true);
        m_sync->heldActivity = message;
        m_sync->heldLimit = limit;
        return {};
    }
    return m_sync->log.mid(0, int(limit));
}

/// One account's object, /org/konedrive/Accounts/<id>, carrying every interface.
class FakeAccountObject : public QObject
{
    Q_OBJECT

public:
    FakeAccountObject(const QString &id, const QString &label, QObject *parent)
        : QObject(parent)
        , id(id)
        , path(fake::accountPath(id))
        , account(new FakeAccount(this, fake::bus(), path, id, label))
        , sync(new FakeSync(this, fake::bus(), path))
    {
    }
    ~FakeAccountObject() override { delete sync; }

    const QString id;
    const QString path;
    FakeAccount *account;
    FakeSync *sync;
};

class FakeDaemon;

/// Accounts on the manager object; its methods act on the FakeDaemon.
class FakeAccounts : public QDBusAbstractAdaptor
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.konedrive.Accounts")
    Q_PROPERTY(QList<QDBusObjectPath> List READ accounts)
    Q_PROPERTY(QString ClientId READ clientId)
    Q_PROPERTY(QString HelperState READ helperState)
    Q_PROPERTY(QString LastError READ lastError)
    Q_PROPERTY(bool PauseOnMetered READ pauseOnMetered)
    Q_PROPERTY(QString OnBattery READ onBattery)
    Q_PROPERTY(QString Version READ version)
    Q_PROPERTY(QString Commit READ commit)

public:
    explicit FakeAccounts(FakeDaemon *daemon);

    QList<QDBusObjectPath> accounts() const;
    QString clientId() const { return m_properties.value(QStringLiteral("ClientId")).toString(); }
    QString helperState() const { return m_properties.value(QStringLiteral("HelperState")).toString(); }
    QString lastError() const { return m_properties.value(QStringLiteral("LastError")).toString(); }
    bool pauseOnMetered() const { return m_properties.value(QStringLiteral("PauseOnMetered")).toBool(); }
    QString onBattery() const { return m_properties.value(QStringLiteral("OnBattery")).toString(); }
    QString version() const { return m_properties.value(QStringLiteral("Version")).toString(); }
    QString commit() const { return m_properties.value(QStringLiteral("Commit")).toString(); }

    /// The daemon's build. Constant on the bus, as the daemon's are: a window
    /// sees a new one only once the daemon comes back (stop, then start).
    void setBuild(const QString &version, const QString &commit)
    {
        m_properties.insert(QStringLiteral("Version"), version);
        m_properties.insert(QStringLiteral("Commit"), commit);
    }

    void set(const QVariantMap &changes)
    {
        for (auto it = changes.cbegin(); it != changes.cend(); ++it) {
            m_properties.insert(it.key(), it.value());
        }
        fake::propertiesChanged(fake::bus(), fake::ManagerPath, DaemonController::InterfaceName, changes);
    }

    QStringList calls;
    /// Remove refuses NoHelper, as it does for an intercepted folder with no helper.
    bool refuseRemove = false;
    /// SignIn is refused Failed with this message, when it is not empty.
    QString refuseSignIn;
    /// An outcome and its message: SignIn's sign-in ends so before SignIn has
    /// answered, so that SignInFinished is sent ahead of the reply.
    QStringList finishBeforeReply;
    /// CancelSignIn ends the sign-in under way "cancelled". Off: it is only
    /// logged, as with a daemon that has not got to it yet.
    bool cancelEnds = true;

public Q_SLOTS:
    uint SignIn(const QDBusMessage &message, QString &url);
    void CancelSignIn(uint signIn);
    void Remove(const QDBusObjectPath &account, const QDBusMessage &message);
    void SetClientId(const QString &id, const QDBusMessage &message)
    {
        calls << QStringLiteral("SetClientId:") + id;
        if (id == QLatin1String("bad")) {
            message.setDelayedReply(true);
            fake::bus().send(message.createErrorReply(QStringLiteral("org.freedesktop.DBus.Error.InvalidArgs"), QStringLiteral("invalid client ID")));
            return;
        }
        set({{QStringLiteral("ClientId"), id}});
    }
    void SetPauseOnMetered(bool on)
    {
        calls << QStringLiteral("SetPauseOnMetered:") + (on ? QStringLiteral("on") : QStringLiteral("off"));
        set({{QStringLiteral("PauseOnMetered"), on}});
    }
    /// Refused InvalidArgs for anything but sync, power-saver or pause, as the daemon does.
    void SetOnBattery(const QString &choice, const QDBusMessage &message)
    {
        calls << QStringLiteral("SetOnBattery:") + choice;
        if (choice != QLatin1String("sync") && choice != QLatin1String("power-saver") && choice != QLatin1String("pause")) {
            message.setDelayedReply(true);
            fake::bus().send(message.createErrorReply(QStringLiteral("org.freedesktop.DBus.Error.InvalidArgs"), QStringLiteral("not a choice: ") + choice));
            return;
        }
        set({{QStringLiteral("OnBattery"), choice}});
    }

private:
    FakeDaemon *m_daemon;
    QVariantMap m_properties{
        {QStringLiteral("ClientId"), QString()},
        {QStringLiteral("HelperState"), QStringLiteral("connected")},
        {QStringLiteral("LastError"), QString()},
        {QStringLiteral("PauseOnMetered"), true},
        {QStringLiteral("OnBattery"), QStringLiteral("power-saver")},
        // By default the same build as the window's.
        {QStringLiteral("Version"), QStringLiteral(KONEDRIVE_VERSION)},
        {QStringLiteral("Commit"), QStringLiteral(KONEDRIVE_COMMIT)},
    };
};

/// The daemon: the manager object and its accounts. `account` and `sync` are
/// the first account's, for tests about one account.
class FakeDaemon : public QObject
{
    Q_OBJECT

public:
    /// One account per label, in order; by default one, "Personal".
    explicit FakeDaemon(const QStringList &labels = {QStringLiteral("Personal")})
        : manager(new FakeAccounts(this))
    {
        // Before anything is sent: Transfers is an a(stt).
        registerKonedriveSyncTypes();
        for (const QString &label : labels) {
            addAccount(label);
        }
    }

    /// Takes the daemon's name on the private bus. Returns false on failure.
    bool start()
    {
        auto connection = fake::bus();
        bool ok = connection.registerObject(fake::ManagerPath, this, QDBusConnection::ExportAdaptors);
        for (FakeAccountObject *object : std::as_const(objects)) {
            ok = connection.registerObject(object->path, object, QDBusConnection::ExportAdaptors) && ok;
        }
        m_started = true;
        return connection.registerService(DaemonController::ServiceName) && ok;
    }

    /// Gives the name up, as a daemon that exits does. Its sign-in under way
    /// goes with it, with no SignInFinished.
    void stop()
    {
        auto connection = fake::bus();
        connection.unregisterService(DaemonController::ServiceName);
        for (FakeAccountObject *object : std::as_const(objects)) {
            connection.unregisterObject(object->path);
        }
        signIn = 0;
        connection.unregisterObject(fake::ManagerPath);
        m_started = false;
    }

    /// An account that is there under `label`, signed out unless `properties`
    /// say otherwise: exported, then announced in Accounts.
    FakeAccountObject *addAccount(const QString &label, const QVariantMap &properties = {})
    {
        auto *object = new FakeAccountObject(fake::idFor(++m_lastId), label, this);
        if (!properties.isEmpty()) {
            object->account->set(properties);
        }
        objects << object;
        if (objects.size() == 1) {
            account = object->account;
            sync = object->sync;
        }
        if (m_started) {
            fake::bus().registerObject(object->path, object, QDBusConnection::ExportAdaptors);
        }
        announce();
        return object;
    }

    /// As Accounts.SignIn does: a sign-in under way, with the next number, and
    /// no account. One at a time: one that is under way ends "cancelled" first.
    uint beginSignIn()
    {
        if (signIn != 0) {
            finishSignIn(QStringLiteral("cancelled"), QString());
        }
        signIn = ++m_lastSignIn;
        return signIn;
    }

    /// How the daemon ends the sign-in under way, with Accounts.SignInFinished
    /// last. "signed-in": the account is made, signed in and called `message`,
    /// its email, and joins Accounts first; the signal names its path.
    /// "already-added": the signal names the account called `message`. Any
    /// other outcome: no account, and "/".
    void finishSignIn(const QString &outcome, const QString &message)
    {
        const uint number = signIn;
        if (number == 0) {
            return;
        }
        signIn = 0;
        QString path = QStringLiteral("/");
        if (outcome == QLatin1String("signed-in")) {
            path = addAccount(message, {{QStringLiteral("Email"), message}, {QStringLiteral("State"), QStringLiteral("signed-in")}})->path;
        } else if (outcome == QLatin1String("already-added")) {
            for (const FakeAccountObject *object : std::as_const(objects)) {
                if (object->account->label() == message) {
                    path = object->path;
                }
            }
        }
        sendFinished(number, outcome, message, path);
    }

    /// Accounts.SignInFinished as it is, for any number.
    void sendFinished(uint number, const QString &outcome, const QString &message, const QString &path)
    {
        auto signal = QDBusMessage::createSignal(fake::ManagerPath, DaemonController::InterfaceName, QStringLiteral("SignInFinished"));
        signal << number << outcome << message << QVariant::fromValue(QDBusObjectPath(path));
        fake::bus().send(signal);
    }

    /// As Accounts.Remove does, once it has forgotten the folder: signed out
    /// first (the daemon's `retire`), then unexported, then gone from Accounts.
    bool removeAccount(const QString &path)
    {
        for (FakeAccountObject *object : std::as_const(objects)) {
            if (object->path == path) {
                object->account->set({{QStringLiteral("State"), QStringLiteral("signed-out")},
                                      {QStringLiteral("DisplayName"), QString()},
                                      {QStringLiteral("Email"), QString()}});
                objects.removeOne(object);
                if (m_started) {
                    fake::bus().unregisterObject(path);
                }
                announce();
                object->deleteLater();
                return true;
            }
        }
        return false;
    }

    FakeAccountObject *object(int index) const { return objects.value(index); }

    FakeAccounts *manager;
    QList<FakeAccountObject *> objects;
    /// The number of the SignIn that has not ended; 0 when none is under way.
    uint signIn = 0;
    FakeAccount *account = nullptr;
    FakeSync *sync = nullptr;

private:
    void announce() { manager->set({{QStringLiteral("List"), QVariant::fromValue(manager->accounts())}}); }

    bool m_started = false;
    int m_lastId = 0;
    uint m_lastSignIn = 0;
};

inline FakeAccounts::FakeAccounts(FakeDaemon *daemon)
    : QDBusAbstractAdaptor(daemon)
    , m_daemon(daemon)
{
}

inline QList<QDBusObjectPath> FakeAccounts::accounts() const
{
    QList<QDBusObjectPath> paths;
    for (const FakeAccountObject *object : std::as_const(m_daemon->objects)) {
        paths << QDBusObjectPath(object->path);
    }
    return paths;
}

inline uint FakeAccounts::SignIn(const QDBusMessage &message, QString &url)
{
    calls << QStringLiteral("SignIn");
    if (!refuseSignIn.isEmpty()) {
        message.setDelayedReply(true);
        fake::bus().send(message.createErrorReply(QStringLiteral("org.konedrive.Error.Failed"), refuseSignIn));
        return 0;
    }
    const uint number = m_daemon->beginSignIn();
    url = QStringLiteral("https://login.example/authorize?sign_in=%1").arg(number);
    if (!finishBeforeReply.isEmpty()) {
        m_daemon->finishSignIn(finishBeforeReply.at(0), finishBeforeReply.at(1));
    }
    return number;
}

/// Never refused; a number that is not under way is ignored. The outcome is
/// sent after the call has been answered.
inline void FakeAccounts::CancelSignIn(uint signIn)
{
    calls << QStringLiteral("CancelSignIn:%1").arg(signIn);
    if (!cancelEnds) {
        return;
    }
    QTimer::singleShot(0, m_daemon, [daemon = m_daemon, signIn] {
        if (daemon->signIn == signIn) {
            daemon->finishSignIn(QStringLiteral("cancelled"), QString());
        }
    });
}

inline void FakeAccounts::Remove(const QDBusObjectPath &account, const QDBusMessage &message)
{
    calls << QStringLiteral("Remove:") + account.path();
    if (refuseRemove) {
        message.setDelayedReply(true);
        fake::bus().send(message.createErrorReply(QStringLiteral("org.konedrive.Error.NoHelper"), QStringLiteral("the konedrive helper is not connected")));
        return;
    }
    if (!m_daemon->removeAccount(account.path())) {
        message.setDelayedReply(true);
        fake::bus().send(message.createErrorReply(QStringLiteral("org.konedrive.Error.NoAccount"), QStringLiteral("no such account")));
    }
}

/// A system tray's StatusNotifierWatcher with a host registered, as Plasma
/// runs one. Tray icons register with it; `items` records their names.
class FakeTrayWatcher : public QObject
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.kde.StatusNotifierWatcher")
    Q_PROPERTY(bool IsStatusNotifierHostRegistered READ hostRegistered)
    Q_PROPERTY(int ProtocolVersion READ protocolVersion)
    Q_PROPERTY(QStringList RegisteredStatusNotifierItems READ registeredItems)

public:
    static inline const QString Service = QStringLiteral("org.kde.StatusNotifierWatcher");
    static inline const QString Path = QStringLiteral("/StatusNotifierWatcher");

    ~FakeTrayWatcher() override
    {
        if (m_started) {
            stop();
        }
    }

    bool start(QDBusConnection connection)
    {
        m_connection = connection;
        m_started = true;
        return connection.registerObject(Path, this, QDBusConnection::ExportAllSlots | QDBusConnection::ExportAllProperties | QDBusConnection::ExportAllSignals)
            && connection.registerService(Service);
    }

    void stop()
    {
        m_connection.unregisterService(Service);
        m_connection.unregisterObject(Path);
        m_started = false;
    }

    bool hostRegistered() const { return true; }
    int protocolVersion() const { return 0; }
    QStringList registeredItems() const { return items; }

    QStringList items;

public Q_SLOTS:
    void RegisterStatusNotifierItem(const QString &service, const QDBusMessage &message)
    {
        // An item may pass its object path instead of a name; the sender is the name then.
        items << (service.startsWith(QLatin1Char('/')) ? message.service() : service);
        Q_EMIT StatusNotifierItemRegistered(items.constLast());
    }
    void RegisterStatusNotifierHost(const QString &) { }

Q_SIGNALS:
    void StatusNotifierItemRegistered(const QString &service);
    void StatusNotifierItemUnregistered(const QString &service);
    void StatusNotifierHostRegistered();
    void StatusNotifierHostUnregistered();

private:
    QDBusConnection m_connection{QString()};
    bool m_started = false;
};
