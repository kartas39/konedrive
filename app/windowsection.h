#pragma once

#include <QString>

/// Where a click on a notification opens the window: Status, its History, or a section of
/// Issues.
enum class WindowSection {
    Status,
    /// Status with its History opened: what happened to the files.
    History,
    /// Issues: the changes that can't upload.
    CantUpload,
    /// Issues: the files whose both versions were kept.
    KeptBothVersions,
    /// Issues: what can't be shown in the folder.
    NotInFolder,
};

/// `section` by the name the window's showSection() takes.
inline QString sectionName(WindowSection section)
{
    switch (section) {
    case WindowSection::Status:
        return QStringLiteral("status");
    case WindowSection::History:
        return QStringLiteral("history");
    case WindowSection::CantUpload:
        return QStringLiteral("cantUpload");
    case WindowSection::KeptBothVersions:
        return QStringLiteral("keptBothVersions");
    case WindowSection::NotInFolder:
        return QStringLiteral("notInFolder");
    }
    return QStringLiteral("status");
}
