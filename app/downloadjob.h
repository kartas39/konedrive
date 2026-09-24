#pragma once

#include <KJob>

#include <QString>

/// One download shown to Plasma, the same mechanism Dolphin's copy
/// progress uses. Never started with start(): DownloadProgressController
/// drives its progress from Sync1's Transfers, so it is created already
/// running and only ever finished by.
class DownloadJob : public KJob
{
    Q_OBJECT

public:
    explicit DownloadJob(QObject *parent = nullptr);

    /// Nothing to trigger: feeds this job its progress.
    void start() override { }

    /// "Downloading from OneDrive", with `fileName` as the description.
    void setFileDescription(const QString &fileName);
    /// The summed job past the cap: "and N more files".
    void setOverflowDescription(int moreCount);
    /// What setFileDescription/setOverflowDescription last put in the
    /// description (a convenience for tests; Plasma gets it through the
    /// description signal).
    QString detailText() const { return m_detailText; }

    /// Bytes done and total; total 0 means unknown, so no percentage shows
    /// (spec: a job's total is the transfer's total; unknown total shows no
    /// percentage). `nowMs` is the injected clock, used to derive speed from
    /// the previous sample.
    void updateProgress(qulonglong done, qulonglong total, qint64 nowMs);

    /// The transfer left Transfers: done.
    void finishSuccess();
    /// A failed/update-failed ActivityAdded named this job's file.
    void finishError(const QString &reason);

protected:
    /// Lets kill(KJob::Quietly) actually end and free the job (M1): the base
    /// class's default refuses (returns false), so without this override
    /// turning the progress switch off left every job frozen and leaked.
    bool doKill() override;

private:
    QString m_detailText;
    bool m_hasSample = false;
    qulonglong m_lastDone = 0;
    qint64 m_lastSampleMs = 0;
};
