use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use crate::upload::{Engine, Limits, OutboxHost, WorkerConfig};
use crate::folder::root::SyncRoot;
use crate::folder::locks::InodeLocks;
use konedrive_tree::{ActivityRow, Store};

use super::FakeGraph;

/// Records what the worker tells its host.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Recorder {
    pub events: Mutex<Vec<ActivityRow>>,
    pub cycles: AtomicUsize,
    /// Cycles asked for with a Full reconcile.
    pub fulls: AtomicUsize,
    /// Why the write gate is closed; open while `None`.
    pub gate: Mutex<Option<String>>,
    /// What a test does whenever the write gate is asked — a row asks it
    /// before each fragment it sends, so this is how a test acts between two
    /// fragments.
    pub asked: Mutex<Vec<Box<dyn FnMut() + Send>>>,
}

#[cfg(test)]
impl OutboxHost for Recorder {
    fn activity(&self, event: &ActivityRow) {
        self.events.lock().unwrap().push(event.clone());
    }

    fn cycle_wanted(&self) {
        self.cycles.fetch_add(1, Ordering::SeqCst);
    }

    fn full_cycle_wanted(&self) {
        self.fulls.fetch_add(1, Ordering::SeqCst);
    }

    fn may_write(&self) -> Result<(), String> {
        for then in self.asked.lock().unwrap().iter_mut() {
            then();
        }
        self.gate.lock().unwrap().clone().map_or(Ok(()), Err)
    }
}

#[cfg(test)]
impl Recorder {
    pub fn kinds(&self) -> Vec<String> {
        self.events.lock().unwrap().iter().map(|e| e.kind.clone()).collect()
    }
}

#[cfg(test)]
/// A worker for one folder against a fake OneDrive, driven by hand: each
/// [`engine`](Harness::engine) is a fresh start on the same store.
pub(crate) struct Harness {
    pub runtime: tokio::runtime::Runtime,
    pub graph: FakeGraph,
    pub host: Arc<Recorder>,
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
    root: SyncRoot,
    store: Store,
    locks: InodeLocks,
    pub limits: Limits,
    /// The helper and the fills a `move-out` row needs; `None` by default.
    pub moved_out: Mutex<Option<crate::upload::move_out::MoveOuts>>,
    /// The account's one quota, which every start shares, as the daemon's
    /// workers share their account's.
    pub quota: crate::account::quota::Quota,
}

#[cfg(test)]
impl Harness {
    /// The fake OneDrive starts as the base is.
    pub fn new(root: &SyncRoot, store: &Store, locks: &InodeLocks) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let graph = runtime.block_on(FakeGraph::from_store(store));
        Self {
            runtime,
            graph,
            host: Arc::new(Recorder::default()),
            tree_lock: Arc::new(tokio::sync::Mutex::new(())),
            root: root.clone(),
            store: store.clone(),
            locks: locks.clone(),
            limits: Limits { chunk: 320 * 1024 },
            moved_out: Mutex::new(None),
            quota: crate::account::quota::Quota::detached(),
        }
    }

    pub fn config(&self) -> WorkerConfig {
        WorkerConfig {
            root: self.root.clone(),
            store: self.store.clone(),
            drive: self.graph.client(),
            locks: self.locks.clone(),
            machine_name: "fedora".into(),
            tree_lock: Arc::clone(&self.tree_lock),
            host: self.host.clone(),
            limits: self.limits,
            moved_out: self.moved_out.lock().unwrap().clone(),
            quota: self.quota.clone(),
        }
    }

    /// A worker as a new daemon start would build it.
    pub fn engine(&self) -> Arc<Engine> {
        Arc::new(Engine::new(self.config()))
    }

    /// Runs `future` on the harness's runtime.
    pub fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.runtime.block_on(future)
    }

    pub fn drain(&self, engine: &Arc<Engine>) {
        self.runtime.block_on(engine.drain(&CancellationToken::new()));
    }

    /// A fresh worker, run until nothing more can run.
    pub fn run(&self) -> Arc<Engine> {
        let engine = self.engine();
        self.drain(&engine);
        engine
    }
}
