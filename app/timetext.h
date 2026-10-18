#pragma once

#include <QDateTime>
#include <QLocale>
#include <QString>

/// A moment as the window writes it, against `now` (both unix seconds): the time of day when
/// it is the same day, else a short date and time.
inline QString timeText(qint64 unixSeconds, qint64 now)
{
    const QDateTime when = QDateTime::fromSecsSinceEpoch(unixSeconds);
    if (when.date() == QDateTime::fromSecsSinceEpoch(now).date()) {
        return QLocale().toString(when.time(), QLocale::ShortFormat);
    }
    return QLocale().toString(when, QLocale::ShortFormat);
}
