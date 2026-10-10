//! A cycle of a read-only folder beside the folder's write-operation thread: the parts of a
//! mount built over a folder that no mount lies over, and a listing whose OneDrive is the
//! fake one.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::conditions::running::Running;
use crate::fake_onedrive::FakeGraph;
use crate::folder::root::SyncRoot;
use crate::fuse::{build_mount, BuiltMount};
use crate::remote::listing::{Lease, Listing, ListingContext};
use crate::remote::mode::Mode;
use crate::status::report::Report;
use crate::status::snapshot::{SyncSnapshot, SyncStateHandle};

/// The listing of the read-only folder of `built`, whose OneDrive is `graph`.
fn read_only_listing(built: &BuiltMount, graph: &FakeGraph) -> Arc<Listing> {
    Listing::new(ListingContext {
        root: SyncRoot { path: built.disk.clone(), root_id: "R".into() },
        store: built.store.clone(),
        drive: graph.client(),
        drive_record: None,
        mount: Arc::clone(&built.mount),
        state: SyncStateHandle::new(SyncSnapshot::default()),
        lease: Lease::on(&Arc::new(tokio::sync::RwLock::new(()))),
        after_cycle: None,
        report: Report::nowhere(),
        mode: Mode::ReadOnly,
        tree_lock: Arc::clone(&built.tree_lock),
        neighbours: None,
        running: Arc::new(Running::default()),
    })
}

/// A job runs on the write-operation thread of a read-only folder — a landing, a free-up —
/// when a cycle comes with another content of a file: the cycle makes the folder follow
/// OneDrive only once the job has ended, as a read-write folder's does. The job reads the
/// file's copy for as long as it runs, and finds it there.
#[test]
fn a_cycle_of_a_read_only_folder_changes_nothing_beside_a_job_of_the_write_operation_thread() {
    let built = build_mount();
    built.download("a.txt", "A", b"old");
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let graph = runtime.block_on(FakeGraph::from_store(&built.store));
    graph.with(|cloud| cloud.edit("A", b"another content"));
    let listing = read_only_listing(&built, &graph);
    let (begin, began) = mpsc::channel();
    let copy = built.disk.join("a.txt");
    let in_flight = built.queue_on_write_operation_thread(move || {
        begin.send(()).unwrap();
        // For as long as the job runs, the copy is there: no cycle removes it meanwhile.
        let until = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < until && copy.exists() {
            std::thread::sleep(Duration::from_millis(10));
        }
        copy.exists()
    });
    began.recv_timeout(Duration::from_secs(5)).unwrap();
    let cycle = runtime.spawn(async move { listing.cycle(&CancellationToken::new()).await });
    assert_eq!(in_flight.blocking_recv(), Ok(true), "the cycle removed the copy while a job ran on the write-operation thread");
    runtime.block_on(cycle).unwrap().unwrap();
    assert!(!built.disk.join("a.txt").exists(), "the cycle made the folder follow OneDrive once the job had ended");
    built.check_rows_agree();
}
