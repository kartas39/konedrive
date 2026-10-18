#pragma once

#include <QDir>
#include <QString>

/// A folder's path as the window writes it: the home directory, and what is under it, from ~.
inline QString folderText(const QString &path, const QString &home = QDir::homePath())
{
    if (home.isEmpty() || home == QLatin1String("/")) {
        return path;
    }
    if (path == home) {
        return QStringLiteral("~");
    }
    if (path.startsWith(home + QLatin1Char('/'))) {
        return QLatin1Char('~') + path.mid(home.size());
    }
    return path;
}
