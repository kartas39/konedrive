//! The blocking sections of a row's step, and the worker's count of them.

use std::io;
use std::sync::Arc;

use crate::upload::engine::{Engine, Fail};
use crate::upload::local;

tokio::task_local! {
    /// The sections of the worker whose row runs on this task ([`Sections::of`]).
    static SECTIONS: Sections;
}

/// The blocking sections a worker's rows have under way. A row's task is
/// dropped where it waits when the worker stops, and a section it waits for
/// goes on without it; the worker's stop waits for those ([`Sections::ended`]),
/// so that nothing of the worker touches the folder or the store once `stop`
/// has returned, as when the file calls ran on the row's own task.
#[derive(Clone, Default)]
pub(crate) struct Sections(Arc<tokio::sync::RwLock<()>>);

impl Sections {
    /// Runs `row`, a row's step, with its sections counted here.
    pub(in crate::upload) async fn of<T>(&self, row: impl std::future::Future<Output = T>) -> T {
        SECTIONS.scope(self.clone(), row).await
    }

    /// A share for one section, let go of when the section ends: [`ended`](Self::ended)
    /// waits for it.
    pub(in crate::upload) async fn running(&self) -> tokio::sync::OwnedRwLockReadGuard<()> {
        Arc::clone(&self.0).read_owned().await
    }

    /// Done once no section is running. For after the rows' tasks ended: no
    /// new section starts then.
    pub(in crate::upload) async fn ended(&self) {
        let _ = self.0.write().await;
    }
}

/// A share in the sections of the worker whose row runs on this task, for
/// work that starts blocking sections of its own (a fill): the worker's stop
/// waits until it is let go of (`folder::locks::holding_with`).
pub(in crate::upload) async fn share() -> Option<crate::folder::locks::Carried> {
    let sections = SECTIONS.try_with(Sections::clone).ok()?;
    Some(Arc::new(sections.running().await))
}

/// `f` on a blocking thread: a *section*, the file calls of a step that
/// follow each other with no wait between them. None of them runs on a
/// runtime thread. A section that has begun runs to its end, whatever
/// becomes of the row's task, and the worker's stop waits for it
/// ([`Sections`]). A failure to run it is an
/// `io::Error`, like its own.
pub(in crate::upload) async fn off<T: Send + 'static>(f: impl FnOnce() -> io::Result<T> + Send + 'static) -> io::Result<T> {
    let running = match SECTIONS.try_with(Sections::clone) {
        Ok(sections) => Some(sections.running().await),
        Err(_) => None,
    };
    tokio::task::spawn_blocking(move || {
        // Let go of last: after the locks the section holds.
        let _running = running;
        f()
    })
    .await
    .map_err(io::Error::other)?
}

/// [`off`] for a step: its failure is the step's.
pub(in crate::upload) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> io::Result<T> + Send + 'static) -> Result<T, Fail> {
    off(f).await.map_err(Fail::Io)
}

/// [`blocking`] under a lock: `hold`, a share in its guard, is let go of
/// when the section is over, not when the row's task is. Whoever takes the
/// lock next never finds a section still at work on the file.
pub(in crate::upload) async fn blocking_under<H: Send + 'static, T: Send + 'static>(hold: H, f: impl FnOnce() -> io::Result<T> + Send + 'static) -> Result<T, Fail> {
    blocking(move || {
        let _hold = hold;
        local::under_lock(f)
    })
    .await
}

/// The tree lock as a step holds it: shared with the sections that change
/// the folder under it ([`blocking_under`]).
pub(in crate::upload) type Tree = Arc<tokio::sync::OwnedMutexGuard<()>>;

pub(in crate::upload) async fn tree(e: &Engine) -> Tree {
    Arc::new(Arc::clone(e.tree_lock()).lock_owned().await)
}
