//! The worker, end to end (`docs/design/writes.md` §12): rows recorded as the mount's write
//! operations record them, in a folder that stands for the backing directory, sent to a
//! fake OneDrive (wiremock, never a network), and the folder, the store and the drive
//! compared afterwards. Nothing is mounted, and no daemon runs. Crashes are fault points:
//! the worker stops at the step, and a new one is built on the same store and folder.
//!
//! [`world`] is the fixture, [`write_operations`] what a test does in the folder, [`harness`]
//! the worker driven by hand. The tests, by what they are about:
//!
//! - [`row_kinds`]: each kind of row from its recording to its commit, and its replay after
//!   a crash;
//! - [`conflicts`]: what OneDrive changed under a row;
//! - [`temporary_names`]: names exchanged, which go through a temporary name;
//! - [`waits`]: throttling, the pause, the write gate, blocked rows, failures in backoff;
//! - [`space_left`]: a full OneDrive, and a file too big for what is left;
//! - [`sessions`]: upload sessions, and the empty placeholder an open one holds its name
//!   with in OneDrive;
//! - [`interrupted_sessions`]: a session interrupted by a pause, a stop or a full OneDrive;
//! - [`stops`]: the worker stopped in the middle of a step;
//! - [`removed`]: a file or directory removed before its upload finished;
//! - [`candidates`]: what a start lets go, and a bad upload;
//! - [`worker`]: the worker's loop;
//! - [`foreign_parent`]: a directory that carries the id of another.

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use konedrive_fs::placeholder::{self, XATTR_CTAG, XATTR_ITEM_ID, XATTR_STAMP, XATTR_STATE, XATTR_SYNC};
use konedrive_tree::outbox::{OutboxKind, OutboxState, Reason};
use konedrive_tree::{ActivityKind, Change, Table};
use tokio_util::sync::CancellationToken;
use wiremock::ResponseTemplate;

use super::*;
use crate::fake_onedrive::{self as fake, qx};
use world::{file, folder, World};

use OutboxKind::{Create, Mkdir, Move, Update};

pub(crate) mod harness;
mod world;
mod write_operations;

mod candidates;
mod conflicts;
mod foreign_parent;
mod interrupted_sessions;
mod removed;
mod row_kinds;
mod sessions;
mod space_left;
mod stops;
mod temporary_names;
mod waits;
mod worker;

/// What a commit leaves on a file whose content went up (§5.4): the item id, `hydrated`,
/// the cTag OneDrive gave, the stamp of the content sent, and no upload mark; on a
/// directory, the item id. The base has the item at its place.
fn assert_committed(w: &World, rel: &str, cloud_path: &str) {
    assert_placed(w, rel, cloud_path);
    let meta = std::fs::metadata(w.path(rel)).unwrap();
    if meta.is_file() {
        assert_eq!(w.attr(rel, XATTR_STATE).as_deref(), Some("hydrated"), "{rel}");
        assert_eq!(w.attr(rel, XATTR_CTAG), w.cloud(|c| c.at(cloud_path).map(|i| i.ctag.clone())), "{rel}");
        assert_eq!(w.attr(rel, XATTR_STAMP), Some(format!("{} {}.{}", meta.len(), meta.mtime(), meta.mtime_nsec())), "{rel}");
        let here = qx(&std::fs::read(w.path(rel)).unwrap());
        assert_eq!(w.cloud(|c| c.at(cloud_path).and_then(|i| i.hash.clone())), Some(here), "{rel}: the content");
    }
}

/// What the commit of a row that sent no content leaves: the entry at `rel` is marked with
/// the id of the item OneDrive has at `cloud_path`, carries no upload mark, and the base has
/// the item at that place.
fn assert_placed(w: &World, rel: &str, cloud_path: &str) {
    let id = w.id_at(cloud_path).unwrap_or_else(|| panic!("{cloud_path} is not in OneDrive: {:?}", w.cloud(|c| c.paths())));
    assert_eq!(w.attr(rel, XATTR_ITEM_ID).as_deref(), Some(id.as_str()), "{rel}");
    assert_eq!(w.attr(rel, XATTR_SYNC), None, "{rel}");
    assert_eq!(w.place_of(&id).as_deref(), Some(std::path::Path::new(cloud_path)), "{rel}: where the base has it");
}

/// A worker draining in the background, with the first request of `method` whose path
/// holds `fragment` held for a while: a test acts on the folder while that request is in
/// flight, then [`finish`](InFlight::finish)es the drain.
struct InFlight(tokio::task::JoinHandle<()>);

impl InFlight {
    fn start(w: &World, method: &str, fragment: &str) -> Self {
        w.cloud(|c| c.delay(method, fragment, Duration::from_millis(400), 1));
        let engine = w.h.engine();
        let task = w.h.runtime.spawn(async move { engine.drain(&CancellationToken::new()).await });
        let mut waited = 0;
        while w.cloud(|c| c.count(method, fragment)) == 0 && waited < 500 {
            std::thread::sleep(Duration::from_millis(10));
            waited += 1;
        }
        assert_eq!(w.cloud(|c| c.count(method, fragment)), 1, "{method} {fragment} is in flight");
        InFlight(task)
    }

    fn finish(self, w: &World) {
        w.h.runtime.block_on(self.0).unwrap();
    }
}

fn reason_of(w: &World, rel: &str) -> Option<String> {
    w.row_at(rel).and_then(|r| r.reason_text())
}

/// The user pauses the account, until resumed: the worker's host says so, as the
/// account's settings do in the daemon, to every worker of the folder.
/// The user pauses the account, until resumed: the worker's host says so, as the
/// account's settings do in the daemon, to every worker of the folder.
fn pause(w: &World) {
    *w.h.host.paused.lock().unwrap() = Some(0);
}

fn resume(w: &World) {
    *w.h.host.paused.lock().unwrap() = None;
}

/// `Refresh()`'s quota read (`SyncService::refresh_quota`), applied.
fn refresh(w: &World, engine: &Arc<Engine>) {
    let quota = w.h.runtime.block_on(w.h.graph.client().quota()).unwrap();
    w.h.block_on(engine.apply_quota(&quota));
}

/// The contents of every request that sent `name`'s content.
fn content_requests(w: &World, name: &str) -> usize {
    w.cloud(|c| c.log.iter().filter(|(m, p)| p.contains(name) && (m == "PUT" || p.ends_with("createUploadSession"))).count())
}

/// Drains `engine` in the background, pauses it while the first request of
/// `method` whose path holds `fragment` is in flight, runs `meanwhile`, and
/// waits for the drain to end.
fn drain_paused_mid_request(w: &World, engine: &Arc<Engine>, method: &str, fragment: &str, meanwhile: impl FnOnce()) {
    drain_stopped_mid_request(w, engine, method, fragment, || {
        pause(w);
        meanwhile();
    });
}

/// Drains `engine` in the background, runs `stop` while the first request
/// of `method` whose path holds `fragment` is in flight, and waits for the
/// drain to end.
fn drain_stopped_mid_request(w: &World, engine: &Arc<Engine>, method: &str, fragment: &str, stop: impl FnOnce()) {
    w.cloud(|c| c.delay(method, fragment, Duration::from_millis(400), 1));
    let task = w.h.runtime.spawn({
        let engine = Arc::clone(engine);
        async move { engine.drain(&CancellationToken::new()).await }
    });
    let mut waited = 0;
    while w.cloud(|c| c.count(method, fragment)) == 0 && waited < 500 {
        std::thread::sleep(Duration::from_millis(10));
        waited += 1;
    }
    assert_eq!(w.cloud(|c| c.count(method, fragment)), 1, "{method} {fragment} is in flight");
    stop();
    w.h.runtime.block_on(task).unwrap();
}

/// Whether the store has it on record that the local version of `original` was kept as the
/// copy `copy` beside it (§7).
fn kept_as_copy(w: &World, original: &str, copy: &str) -> bool {
    let (original, copy) = (w.path(original).display().to_string(), w.path(copy).display().to_string());
    let kept = w.store.call_blocking(|s| s.conflicts()).unwrap();
    kept.iter().any(|row| row.kind == konedrive_tree::ConflictKind::Copy && row.original == original && row.rescued == copy)
}
