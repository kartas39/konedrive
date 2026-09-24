#include "accountcontroller.h"
#include "activitymodel.h"
#include "appstatus.h"
#include "autostart.h"
#include "conflictmodel.h"
#include "downloadjobtracker.h"
#include "downloadprogresscontroller.h"
#include "downloadprogresssettings.h"
#include "notifier.h"
#include "synccontroller.h"
#include "transfermodel.h"
#include "trayicon.h"

#include <KAboutData>
#include <KDBusService>
#include <KLocalizedQmlContext>
#include <KLocalizedString>
#include <KWindowSystem>

#include <QApplication>
#include <QCommandLineParser>
#include <QIcon>
#include <QQmlApplicationEngine>
#include <QQuickStyle>
#include <QQuickWindow>
#include <QUrl>
#include <QtQml>

int main(int argc, char *argv[])
{
    QApplication app(argc, argv);
    // The window closes to the tray; only the tray's "Quit" ends the app.
    QApplication::setQuitOnLastWindowClosed(false);
    KLocalizedString::setApplicationDomain("konedrive");

    KAboutData about(QStringLiteral("konedrive"),
                     i18nc("@title", "KOneDrive"),
                     QStringLiteral("0.1.0"),
                     i18n("OneDrive account for KDE"),
                     KAboutLicense::GPL_V3);
    // The project is GPL-3.0-or-later; the constructor only takes the key.
    about.setLicense(KAboutLicense::GPL_V3, KAboutLicense::OrLaterVersions);
    // KDBusService's name: org.konedrive.konedrive.
    about.setOrganizationDomain(QByteArrayLiteral("konedrive.org"));
    about.setDesktopFileName(QStringLiteral("org.konedrive.KOneDrive"));
    KAboutData::setApplicationData(about);
    QApplication::setWindowIcon(QIcon::fromTheme(QStringLiteral("folder-cloud")));

    const QCommandLineOption background(QStringLiteral("background"), i18n("Start in the system tray without showing the window."));
    QCommandLineParser parser;
    parser.addOption(background);
    about.setupCommandLine(&parser);
    parser.process(app);
    about.processCommandLine(&parser);

    // A second launch asks this one to show its window, then exits here.
    KDBusService service(KDBusService::Unique);

    if (qEnvironmentVariableIsEmpty("QT_QUICK_CONTROLS_STYLE")) {
        QQuickStyle::setStyle(QStringLiteral("org.kde.desktop"));
    }

    AccountController account;
    SyncController sync;
    AppStatus status(&account, &sync);
    Autostart autostart;
    autostart.applyFirstRunDefault();
    DownloadProgressSettings downloadProgressSettings;
    KUiServerDownloadJobTracker downloadJobTracker;
    DownloadProgressController downloadProgress(&sync, &downloadJobTracker, &downloadProgressSettings);

    qmlRegisterSingletonInstance("org.konedrive.app", 1, 0, "Account", &account);
    qmlRegisterSingletonInstance("org.konedrive.app", 1, 0, "Sync", &sync);
    qmlRegisterSingletonInstance("org.konedrive.app", 1, 0, "Status", &status);
    qmlRegisterSingletonInstance("org.konedrive.app", 1, 0, "Autostart", &autostart);
    qmlRegisterSingletonInstance("org.konedrive.app", 1, 0, "DownloadProgress", &downloadProgressSettings);
    qmlRegisterUncreatableType<TransferModel>("org.konedrive.app", 1, 0, "TransferModel", QStringLiteral("owned by Sync"));
    qmlRegisterUncreatableType<ActivityModel>("org.konedrive.app", 1, 0, "ActivityModel", QStringLiteral("owned by Sync"));
    qmlRegisterUncreatableType<ConflictModel>("org.konedrive.app", 1, 0, "ConflictModel", QStringLiteral("owned by Sync"));

    QQmlApplicationEngine engine;
    KLocalization::setupLocalizedContext(&engine);
    engine.load(QUrl(QStringLiteral("qrc:/Main.qml")));
    if (engine.rootObjects().isEmpty()) {
        return 1;
    }
    auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());

    TrayIcon tray(&status, &sync);
    tray.setWindow(window);
    QObject::connect(&tray, &TrayIcon::quitRequested, &app, &QCoreApplication::quit);

    KNotificationSink sink([&tray] {
        tray.showWindow();
    });
    Notifier notifier(&account, &sync, &sink);

    QObject::connect(&service, &KDBusService::activateRequested, &tray, [&tray, window](const QStringList &arguments, const QString &) {
        // arguments[0] is the program; an autostart while running changes nothing.
        const bool inBackground = arguments.contains(QStringLiteral("--background"));
        qCDebug(KONEDRIVE_APP) << "second launch, background:" << inBackground;
        if (inBackground) {
            return;
        }
        // The launch's activation token, which KDBusService has just put in place.
        KWindowSystem::updateStartupId(window);
        tray.showWindow();
    });

    if (!parser.isSet(background)) {
        tray.showWindow();
    }
    qCDebug(KONEDRIVE_APP) << "ready, window" << (window && window->isVisible() ? "shown" : "hidden");
    return app.exec();
}
