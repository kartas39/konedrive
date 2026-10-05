#include "uploadreasons.h"

#include "generated/reasontexts.h"

#include <QLocale>

QString uploadReasonText(const QString &reason)
{
    const ReasonParts parts = reasonParts(reason);
    const QLocale locale;
    const QString sentence = reasonSentence(parts, locale.formattedDataSize(qint64(parts.needs)), locale.formattedDataSize(qint64(parts.free)));
    return sentence.isEmpty() ? reason : sentence;
}
