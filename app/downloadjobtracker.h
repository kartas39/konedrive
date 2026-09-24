#pragma once

class KJob;

/// Where a promoted download's KJob goes. Tests record registrations
/// instead (ruling: tests must never create a real Plasma job); the
/// app registers with Plasma through KUiServerDownloadJobTracker.
class DownloadJobTracker
{
public:
    virtual ~DownloadJobTracker() = default;
    virtual void registerJob(KJob *job) = 0;
    virtual void unregisterJob(KJob *job) = 0;
};

/// Forwards to a real KUiServerV2JobTracker, the same mechanism that shows
/// Dolphin's copy progress in Plasma.
class KUiServerDownloadJobTracker : public DownloadJobTracker
{
public:
    KUiServerDownloadJobTracker();
    ~KUiServerDownloadJobTracker() override;

    void registerJob(KJob *job) override;
    void unregisterJob(KJob *job) override;

private:
    class KUiServerV2JobTracker *m_tracker;
};
