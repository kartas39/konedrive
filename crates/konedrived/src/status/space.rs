//! `LocalBytes`: the space the folder's files take on disk, measured again
//! after whatever may have changed it.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use crate::folder::walk::walk_files;
use crate::status::snapshot::SyncStateHandle;

/// `LocalBytes` is measured at most this often after a download or a free-up.
pub(crate) const SPACE_SPACING: Duration = Duration::from_secs(5);

/// How `LocalBytes` is measured: a function of the folder's path.
type Measure = Arc<dyn Fn(&Path) -> u64 + Send + Sync>;

/// `LocalBytes`: measured by a walk ([`local_bytes`]) on a
/// blocking thread each time it is [`kick`](Self::kick)ed — after every
/// cycle, download and free-up — but never within [`SPACE_SPACING`] of the
/// last walk: a kick meanwhile makes one more walk when that time is up.
///
/// The walker is a task of its own that holds the sync state, so it is
/// stopped with this (`Drop`), and whenever [`stop`](Self::stop) is asked —
/// a sync that stops, a Forget — rather than left to run for the life of
/// the runtime: nothing waiting on the state would ever see it end. A kick
/// after a stop starts it again.
pub(crate) struct LocalSpace {
    kick: Arc<Notify>,
    state: SyncStateHandle,
    measure: Measure,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Where the folder's files are, when that is not its path: the directory under the
    /// mount that serves it. A walk through the mount would ask the mount
    /// about every name of the drive.
    at: Arc<Mutex<Option<std::path::PathBuf>>>,
    /// False for a report that reports nowhere (`Report::nowhere`): no
    /// walker is ever started for it.
    enabled: bool,
}

impl LocalSpace {
    pub fn new(state: SyncStateHandle) -> Self {
        Self::measuring(state, Arc::new(local_bytes))
    }

    fn measuring(state: SyncStateHandle, measure: Measure) -> Self {
        Self { kick: Arc::new(Notify::new()), state, measure, task: Mutex::new(None), at: Arc::default(), enabled: true }
    }

    #[cfg(test)]
    pub(super) fn inert(state: SyncStateHandle) -> Self {
        let mut space = Self::new(state);
        space.enabled = false;
        space
    }

    #[cfg(test)]
    pub(crate) fn running(&self) -> bool {
        crate::panic::lock(&self.task).as_ref().is_some_and(|task| !task.is_finished())
    }

    /// Stops the walker, if one runs; the next kick starts another.
    pub(crate) fn stop(&self) {
        if let Some(task) = crate::panic::lock(&self.task).take() {
            task.abort();
        }
    }

    /// Asks for a walk. The walker starts with the first kick made on a
    /// runtime, so that nothing is spawned where there is none.
    /// The walk measures `at` from now on, or the folder's own path again (`None`).
    pub(crate) fn measure_at(&self, at: Option<std::path::PathBuf>) {
        *crate::panic::lock(&self.at) = at;
    }

    pub(crate) fn kick(&self) {
        if !self.enabled {
            return;
        }
        {
            let mut task = crate::panic::lock(&self.task);
            if task.is_none() {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    let (kick, state, measure, at) = (Arc::clone(&self.kick), self.state.clone(), Arc::clone(&self.measure), Arc::clone(&self.at));
                    *task = Some(runtime.spawn(walker(kick, state, measure, at)));
                }
            }
        }
        self.kick.notify_one();
    }
}

impl Drop for LocalSpace {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn walker(kick: Arc<Notify>, state: SyncStateHandle, measure: Measure, at: Arc<Mutex<Option<std::path::PathBuf>>>) {
    loop {
        kick.notified().await;
        let root = state.get().folder.root_path;
        let bytes = if root.is_empty() {
            0
        } else {
            let walked = crate::panic::lock(&at).clone().unwrap_or_else(|| std::path::PathBuf::from(&root));
            let measure = Arc::clone(&measure);
            match tokio::task::spawn_blocking(move || measure(&walked)).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!("the walk measuring the folder's space failed: {e}");
                    continue;
                }
            }
        };
        // A folder forgotten, or another registered, while it walked: what
        // it found is not about the folder there is now.
        state.update(|s| {
            if s.folder.root_path == root {
                s.local.local_bytes = bytes;
            }
        });
        tokio::time::sleep(SPACE_SPACING).await;
    }
}

/// `LocalBytes`: what the folder's regular files take on disk, `st_blocks ×
/// 512` — a file counts as what it occupies, not its size.
pub(crate) fn local_bytes(root: &Path) -> u64 {
    let mut total = 0u64;
    walk_files(root, &mut |_, meta| total += meta.blocks() * 512);
    total
}

#[cfg(test)]
mod tests;
