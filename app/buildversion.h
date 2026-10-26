#pragma once

#include <QString>
#include <QStringView>

#include <optional>

/// The version of a KOneDrive build, as it is shown and tagged (docs/releasing.md): a release
/// `X.Y.Z`, its tag `vX.Y.Z`, a package that is not a release `X.Y.Z-dev.N`, and a plain build
/// `X.Y.Z-dev`. A build that is not a release comes before the release of its `X.Y.Z`, as RPM
/// orders `X.Y.Z~dev.N`, and after every earlier release.
struct BuildVersion {
    int major = 0;
    int minor = 0;
    int patch = 0;
    /// Not the release `X.Y.Z` itself but a build on the way to it.
    bool development = false;
    /// The commit count of a package that is not a release; 0 for a plain build.
    int developmentNumber = 0;

    /// `X.Y.Z`, `vX.Y.Z`, `X.Y.Z-dev` or `X.Y.Z-dev.N`; nothing for any other text.
    static std::optional<BuildVersion> parse(QStringView text);
    /// This build comes before `other`.
    bool isOlderThan(const BuildVersion &other) const;
    /// As it is shown: `0.2.2`, `0.2.2-dev`, `0.2.2-dev.57`.
    QString toString() const;
};
