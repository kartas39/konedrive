#include "uploadreasons.h"

#include "generated/reasontexts.h"

#include <QLocale>

#include <limits>

QString uploadReasonText(const QString &reason)
{
    const ReasonParts parts = reasonParts(reason);
    // QLocale writes a signed size: one past what that holds would be printed negative, so
    // such a reason is shown as stored, like any other damaged one.
    const quint64 most = quint64(std::numeric_limits<qint64>::max());
    if (parts.hasSizes && (parts.needs > most || parts.free > most)) {
        return reason;
    }
    const QLocale locale;
    const QString sentence = reasonSentence(parts, locale.formattedDataSize(qint64(parts.needs)), locale.formattedDataSize(qint64(parts.free)));
    return sentence.isEmpty() ? reason : sentence;
}
