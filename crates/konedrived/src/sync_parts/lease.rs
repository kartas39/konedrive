//! The folder's lease, as the plan's task sees it: what a step towards the folder must
//! hold while it changes the folder, so that whoever changes what the folder is (a
//! Forget, a bring-up, a switch of mode) waits for it (`docs/design/sync.md` §9.3).
//! `account_folder/` hands it in; the task never sees what it is a lease on.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::RwLock;

/// Gives out the lease ([`Lease::hold`]).
#[derive(Clone)]
pub(crate) struct Lease(Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Held> + Send>> + Send + Sync>);

/// The lease while it is held: let go of when dropped, on whatever thread that is.
pub(crate) struct Held(#[allow(dead_code)] Box<dyn Send>);

impl Lease {
    /// A lease on `lock`, held for reading. It does not keep `lock` alive: what `lock`
    /// guards may own the very task that holds the lease. Once `lock` is gone there is
    /// nothing left to wait for, and the lease is given at once.
    pub(crate) fn on<T: Send + Sync + 'static>(lock: &Arc<RwLock<T>>) -> Self {
        let lock = Arc::downgrade(lock);
        Self(Arc::new(move || {
            let lock = lock.upgrade();
            Box::pin(async move {
                match lock {
                    Some(lock) => Held(Box::new(lock.read_owned().await)),
                    None => Held(Box::new(())),
                }
            })
        }))
    }

    /// Waits for the lease. The caller gives up when it is cancelled (`cancellable`): a
    /// change of the folder cancels the step before it waits for it.
    pub(crate) async fn hold(&self) -> Held {
        (self.0)().await
    }
}
