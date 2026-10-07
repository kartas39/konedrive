#include "traysettings.h"

#include <KConfig>
#include <KConfigGroup>

#include <QStandardPaths>

namespace
{
const QString ConfigName = QStringLiteral("konedriverc");
const char Group[] = "General";
const char Key[] = "TrayIconPerAccount";

QString configPath()
{
    return QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation) + QLatin1Char('/') + ConfigName;
}
}

TraySettings::TraySettings(QObject *parent)
    : QObject(parent)
{
    KConfig config(configPath(), KConfig::SimpleConfig);
    m_perAccount = config.group(QLatin1String(Group)).readEntry(Key, true);
}

void TraySettings::setPerAccount(bool perAccount)
{
    if (m_perAccount == perAccount) {
        return;
    }
    m_perAccount = perAccount;
    KConfig config(configPath(), KConfig::SimpleConfig);
    config.group(QLatin1String(Group)).writeEntry(Key, perAccount);
    config.sync();
    Q_EMIT perAccountChanged();
}
