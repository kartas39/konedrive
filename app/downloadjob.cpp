#include "downloadjob.h"

#include <KLocalizedString>

DownloadJob::DownloadJob(QObject *parent)
    : KJob(parent)
{
    setCapabilities(NoCapabilities);
    setProgressUnit(Bytes);
}

void DownloadJob::setFileDescription(const QString &fileName)
{
    m_detailText = fileName;
    Q_EMIT description(this, i18nc("@title job", "Downloading from OneDrive"), qMakePair(i18nc("@label", "File"), fileName), {});
}

void DownloadJob::setOverflowDescription(int moreCount)
{
    m_detailText = i18np("and 1 more file", "and %1 more files", moreCount);
    Q_EMIT description(this, i18nc("@title job", "Downloading from OneDrive"), qMakePair(QString(), m_detailText), {});
}

void DownloadJob::updateProgress(qulonglong done, qulonglong total, qint64 nowMs)
{
    if (total > 0) {
        setTotalAmount(Bytes, total);
    }
    setProcessedAmount(Bytes, done);

    if (m_hasSample && nowMs > m_lastSampleMs && done >= m_lastDone) {
        const qulonglong deltaBytes = done - m_lastDone;
        const qint64 deltaMs = nowMs - m_lastSampleMs;
        emitSpeed(static_cast<unsigned long>(deltaBytes * 1000 / static_cast<qulonglong>(deltaMs)));
    }
    m_lastDone = done;
    m_lastSampleMs = nowMs;
    m_hasSample = true;
}

void DownloadJob::finishSuccess()
{
    emitResult();
}

void DownloadJob::finishError(const QString &reason)
{
    setError(KJob::UserDefinedError);
    setErrorText(reason);
    emitResult();
}

bool DownloadJob::doKill()
{
    return true;
}
