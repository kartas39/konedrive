#include "downloadjobtracker.h"

#include <KUiServerV2JobTracker>

KUiServerDownloadJobTracker::KUiServerDownloadJobTracker()
    : m_tracker(new KUiServerV2JobTracker)
{
}

KUiServerDownloadJobTracker::~KUiServerDownloadJobTracker()
{
    delete m_tracker;
}

void KUiServerDownloadJobTracker::registerJob(KJob *job)
{
    m_tracker->registerJob(job);
}

void KUiServerDownloadJobTracker::unregisterJob(KJob *job)
{
    m_tracker->unregisterJob(job);
}
