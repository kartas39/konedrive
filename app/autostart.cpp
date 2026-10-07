#include "autostart.h"

#include <KConfig>
#include <KConfigGroup>
#include <KLocalizedString>

#include <QDir>
#include <QFile>
#include <QFileInfo>
#include <QSaveFile>
#include <QStandardPaths>

#include <algorithm>

namespace
{
const QString ConfigName = QStringLiteral("konedriverc");
const char Group[] = "General";
const char Key[] = "StartAtLogin";

QString configPath()
{
    return QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation) + QLatin1Char('/') + ConfigName;
}

QString quotedArgument(const QString &program);

/// The program as an Exec= argument (Desktop Entry spec: quoted when it
/// holds a reserved character, with ", `, $ and \ escaped; a literal % is
/// %%, since % starts a field code).
QString execArgument(const QString &program)
{
    return quotedArgument(program).replace(QLatin1Char('%'), QStringLiteral("%%"));
}

QString quotedArgument(const QString &program)
{
    static const QString reserved = QStringLiteral(" \t\n\"'\\><~|&;$*?#()`");
    if (std::none_of(program.cbegin(), program.cend(), [](QChar c) {
            return reserved.contains(c);
        })) {
        return program;
    }
    // The string escape (\\ for \) applies before the quoting rule, so an
    // escaped character takes two backslashes, and a backslash four.
    QString quoted;
    for (const QChar c : program) {
        if (c == QLatin1Char('\\')) {
            quoted += QStringLiteral("\\\\\\\\");
            continue;
        }
        if (c == QLatin1Char('"') || c == QLatin1Char('`') || c == QLatin1Char('$')) {
            quoted += QStringLiteral("\\\\");
        }
        quoted += c;
    }
    return QLatin1Char('"') + quoted + QLatin1Char('"');
}

void storeChoice(bool enabled)
{
    KConfig config(configPath(), KConfig::SimpleConfig);
    config.group(QLatin1String(Group)).writeEntry(Key, enabled);
    config.sync();
}
}

Autostart::Autostart(const QString &program, QObject *parent)
    : QObject(parent)
    , m_program(program)
{
}

QString Autostart::defaultProgram()
{
    return QStringLiteral(KONEDRIVE_INSTALLED_PROGRAM);
}

QString Autostart::entryPath()
{
    return QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation) + QStringLiteral("/autostart/org.konedrive.KOneDrive.desktop");
}

bool Autostart::enabled() const
{
    return QFileInfo::exists(entryPath());
}

void Autostart::setEnabled(bool enabled)
{
    const bool before = this->enabled();
    const QString path = entryPath();
    QString failure;
    if (enabled) {
        const QString dir = QFileInfo(path).path();
        QSaveFile file(path);
        if (!QDir().mkpath(dir)) {
            failure = i18n("Could not create the folder %1.", dir);
        } else if (!file.open(QIODevice::WriteOnly)) {
            failure = i18n("Could not write %1: %2", path, file.errorString());
        } else {
            const QString entry = QStringLiteral(
                                      "[Desktop Entry]\n"
                                      "Type=Application\n"
                                      "Name=KOneDrive\n"
                                      "Comment=OneDrive for KDE, in the system tray\n"
                                      "Exec=%1 --background\n"
                                      "Icon=folder-cloud\n"
                                      "Terminal=false\n"
                                      "X-GNOME-Autostart-enabled=true\n")
                                      .arg(execArgument(m_program));
            file.write(entry.toUtf8());
            if (!file.commit()) {
                failure = i18n("Could not write %1: %2", path, file.errorString());
            }
        }
    } else if (QFile entry(path); entry.exists() && !entry.remove()) {
        failure = i18n("Could not remove %1: %2", path, entry.errorString());
    }
    if (failure.isEmpty()) {
        storeChoice(enabled);
    }
    setError(failure);
    // After a failure too: a switch the user flipped goes back to what is true.
    if (this->enabled() != before || !failure.isEmpty()) {
        Q_EMIT enabledChanged();
    }
}

void Autostart::setError(const QString &error)
{
    if (m_error == error) {
        return;
    }
    m_error = error;
    Q_EMIT errorChanged();
}

void Autostart::applyFirstRunDefault()
{
    KConfig config(configPath(), KConfig::SimpleConfig);
    if (config.group(QLatin1String(Group)).hasKey(Key)) {
        return;
    }
    setEnabled(true);
}
