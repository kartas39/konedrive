#pragma once

#include <QDir>
#include <QString>
#include <QStringList>

/// The window's option that opens the picker of the chosen folders for the
/// account whose folder is given (Dolphin's "Choose Folders…").
inline const QString ChooseFoldersOption = QStringLiteral("choose-folders");

/// The folder of `--choose-folders <folder>` (or `--choose-folders=<folder>`)
/// among a launch's arguments, the program's name first: as main() gets them,
/// and as a second launch hands them over (KDBusService::activateRequested).
/// A relative folder is taken from `workingDirectory`, the launch's. Empty
/// when the option is not given, or has no folder.
inline QString chooseFoldersArgument(const QStringList &arguments, const QString &workingDirectory)
{
    const QString option = QStringLiteral("--") + ChooseFoldersOption;
    QString folder;
    for (int i = 1; i < arguments.size(); ++i) {
        const QString &argument = arguments.at(i);
        if (argument == option && i + 1 < arguments.size()) {
            folder = arguments.at(i + 1);
            break;
        }
        if (argument.startsWith(option + QLatin1Char('='))) {
            folder = argument.mid(option.size() + 1);
            break;
        }
    }
    if (folder.isEmpty()) {
        return {};
    }
    return QDir::cleanPath(QDir(workingDirectory.isEmpty() ? QDir::currentPath() : workingDirectory).absoluteFilePath(folder));
}
