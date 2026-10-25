#include "skipreasons.h"

#include <KLocalizedString>

QString skipReasonText(const QString &reason)
{
    if (reason == QLatin1String("name-too-long")) {
        return i18n("The name is longer than Linux allows (255 bytes; a Cyrillic letter takes two).");
    }
    if (reason == QLatin1String("personal-vault")) {
        return i18n("The Personal Vault is locked separately and is not synced.");
    }
    if (reason == QLatin1String("shared")) {
        return i18n("A shared folder added to your OneDrive; shared folders are not synced yet.");
    }
    if (reason == QLatin1String("onenote")) {
        return i18n("A OneNote notebook, which is not a file.");
    }
    if (reason == QLatin1String("reserved-name")) {
        return i18n("The name begins with .konedrive-, which konedrive keeps for itself.");
    }
    return i18n("It is neither a file nor a folder konedrive can show.");
}
