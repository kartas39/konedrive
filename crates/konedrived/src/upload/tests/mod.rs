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
//! - [`space`]: a full OneDrive, and a file too big for what is left;
//! - [`sessions`]: upload sessions;
//! - [`stops`]: the worker stopped in the middle of a step;
//! - [`removed`]: a file or directory removed before its upload finished;
//! - [`candidates`]: what a start lets go, and a bad upload;
//! - [`worker`]: the worker's loop;
//! - [`foreign_parent`]: a directory that carries the id of another.

use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::time::Duration;

use konedrive_fs::placeholder::{XATTR_CTAG, XATTR_ITEM_ID, XATTR_STAMP, XATTR_STATE, XATTR_SYNC};
use tokio_util::sync::CancellationToken;

use super::Engine;
use crate::fake_onedrive::qx;
use world::World;

pub(crate) mod harness;
mod world;
mod write_operations;

mod row_kinds;

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
    fn start(w: &World, engine: &Arc<Engine>, method: &str, fragment: &str) -> Self {
        w.cloud(|c| c.delay(method, fragment, Duration::from_millis(400), 1));
        let engine = Arc::clone(engine);
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
