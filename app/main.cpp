#include "accountcontroller.h"

#include <KAboutData>
#include <KLocalizedQmlContext>
#include <KLocalizedString>

#include <QApplication>
#include <QIcon>
#include <QQmlApplicationEngine>
#include <QQuickStyle>
#include <QUrl>
#include <QtQml>

int main(int argc, char *argv[])
{
    QApplication app(argc, argv);
    KLocalizedString::setApplicationDomain("konedrive");

    KAboutData about(QStringLiteral("konedrive"),
                     i18nc("@title", "KOneDrive"),
                     QStringLiteral("0.1.0"),
                     i18n("OneDrive account for KDE"),
                     KAboutLicense::Unknown);
    about.setDesktopFileName(QStringLiteral("org.konedrive.KOneDrive"));
    KAboutData::setApplicationData(about);
    QApplication::setWindowIcon(QIcon::fromTheme(QStringLiteral("folder-cloud")));

    if (qEnvironmentVariableIsEmpty("QT_QUICK_CONTROLS_STYLE")) {
        QQuickStyle::setStyle(QStringLiteral("org.kde.desktop"));
    }

    AccountController account;
    qmlRegisterSingletonInstance("org.konedrive.app", 1, 0, "Account", &account);

    QQmlApplicationEngine engine;
    KLocalization::setupLocalizedContext(&engine);
    engine.load(QUrl(QStringLiteral("qrc:/Main.qml")));
    if (engine.rootObjects().isEmpty()) {
        return 1;
    }
    return app.exec();
}
