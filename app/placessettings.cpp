#include "placessettings.h"

#include <KConfig>
#include <KConfigGroup>

#include <QStandardPaths>

namespace
{
const QString ConfigName = QStringLiteral("konedriverc");
const char Group[] = "General";
const char Key[] = "ShowInPlaces";

QString configPath()
{
    return QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation) + QLatin1Char('/') + ConfigName;
}
}

PlacesSettings::PlacesSettings(QObject *parent)
    : QObject(parent)
{
    KConfig config(configPath(), KConfig::SimpleConfig);
    m_enabled = config.group(QLatin1String(Group)).readEntry(Key, true);
}

void PlacesSettings::setEnabled(bool enabled)
{
    if (m_enabled == enabled) {
        return;
    }
    m_enabled = enabled;
    KConfig config(configPath(), KConfig::SimpleConfig);
    config.group(QLatin1String(Group)).writeEntry(Key, enabled);
    config.sync();
    Q_EMIT enabledChanged();
}
