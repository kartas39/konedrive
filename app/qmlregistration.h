#pragma once

// What Main.qml and its pages find in `org.konedrive.app`: the singletons
// and the types they reach through them. main() and the dialogs test
// register them the same way.

#include "accountsmodel.h"
#include "accountstatus.h"
#include "activitymodel.h"
#include "autostart.h"
#include "conflictmodel.h"
#include "currentaccount.h"
#include "daemoncontroller.h"
#include "downloadprogresssettings.h"
#include "placessettings.h"
#include "transfermodel.h"

#include <QtQml>

inline void registerKonedriveQml(DaemonController *daemon,
                                 AccountsModel *accounts,
                                 CurrentAccount *current,
                                 Autostart *autostart,
                                 DownloadProgressSettings *downloadProgress,
                                 PlacesSettings *places)
{
    const char *uri = "org.konedrive.app";
    qmlRegisterSingletonInstance(uri, 1, 0, "Daemon", daemon);
    qmlRegisterSingletonInstance(uri, 1, 0, "Accounts", accounts);
    qmlRegisterSingletonInstance(uri, 1, 0, "Current", current);
    qmlRegisterSingletonInstance(uri, 1, 0, "Autostart", autostart);
    qmlRegisterSingletonInstance(uri, 1, 0, "DownloadProgress", downloadProgress);
    qmlRegisterSingletonInstance(uri, 1, 0, "Places", places);
    qmlRegisterUncreatableType<AccountItem>(uri, 1, 0, "AccountItem", QStringLiteral("owned by Accounts"));
    qmlRegisterUncreatableType<AccountController>(uri, 1, 0, "AccountController", QStringLiteral("owned by Accounts"));
    qmlRegisterUncreatableType<SyncController>(uri, 1, 0, "SyncController", QStringLiteral("owned by Accounts"));
    qmlRegisterUncreatableType<AccountStatus>(uri, 1, 0, "AccountStatus", QStringLiteral("owned by Accounts"));
    qmlRegisterUncreatableType<TransferModel>(uri, 1, 0, "TransferModel", QStringLiteral("owned by a SyncController"));
    qmlRegisterUncreatableType<ActivityModel>(uri, 1, 0, "ActivityModel", QStringLiteral("owned by a SyncController"));
    qmlRegisterUncreatableType<ConflictModel>(uri, 1, 0, "ConflictModel", QStringLiteral("owned by a SyncController"));
}
