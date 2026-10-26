#include "buildversion.h"

#include <QRegularExpression>

#include <tuple>

std::optional<BuildVersion> BuildVersion::parse(QStringView text)
{
    static const QRegularExpression shape(QStringLiteral("^v?(\\d{1,6})\\.(\\d{1,6})\\.(\\d{1,6})(-dev(?:\\.(\\d{1,9}))?)?$"));
    const QRegularExpressionMatch match = shape.matchView(text);
    if (!match.hasMatch()) {
        return std::nullopt;
    }
    BuildVersion version;
    version.major = match.capturedView(1).toInt();
    version.minor = match.capturedView(2).toInt();
    version.patch = match.capturedView(3).toInt();
    version.development = match.hasCaptured(4);
    version.developmentNumber = match.hasCaptured(5) ? match.capturedView(5).toInt() : 0;
    return version;
}

bool BuildVersion::isOlderThan(const BuildVersion &other) const
{
    // Within one X.Y.Z: the builds on the way to it, by their number, then the release.
    return std::tuple(major, minor, patch, !development, developmentNumber)
        < std::tuple(other.major, other.minor, other.patch, !other.development, other.developmentNumber);
}

QString BuildVersion::toString() const
{
    QString text = QStringLiteral("%1.%2.%3").arg(major).arg(minor).arg(patch);
    if (development) {
        text += QStringLiteral("-dev");
        if (developmentNumber > 0) {
            text += QStringLiteral(".%1").arg(developmentNumber);
        }
    }
    return text;
}
