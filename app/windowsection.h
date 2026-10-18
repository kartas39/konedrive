#pragma once

#include <QString>

/// Where a click on a notification opens the window: a page, or a section of Issues.
enum class WindowSection {
    Status,
    /// Issues: the changes that can't upload.
    CantUpload,
    /// Issues: the files whose both versions were kept.
    KeptBothVersions,
    /// Issues: what can't be shown in the folder.
    NotInFolder,
    Activity,
    Storage,
};

/// `section` by the name the window's showSection() takes.
inline QString sectionName(WindowSection section)
{
    switch (section) {
    case WindowSection::Status:
        return QStringLiteral("status");
    case WindowSection::CantUpload:
        return QStringLiteral("cantUpload");
    case WindowSection::KeptBothVersions:
        return QStringLiteral("keptBothVersions");
    case WindowSection::NotInFolder:
        return QStringLiteral("notInFolder");
    case WindowSection::Activity:
        return QStringLiteral("activity");
    case WindowSection::Storage:
        return QStringLiteral("storage");
    }
    return QStringLiteral("status");
}
