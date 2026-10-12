//! The owner over a folder whose sync really runs: a sync started over the mount's fixture
//! (`sync_tasks::tests`), with the mount built and not served, held by the service as
//! `Sync::Running`.

mod running_sync;
