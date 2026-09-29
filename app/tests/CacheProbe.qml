import QtQml

/// A resource file outside the window's module: Qt compiles it at run time
/// and writes its unit to the disk cache, which shows qmlcachetest where
/// the cache is and that it is on.
QtObject {
    objectName: "cacheProbe"
}
