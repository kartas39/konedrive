//! The folder's lease, as a cycle sees it: what it must hold while it changes the folder,
//! so that whoever changes what the folder is (a Forget, a bring-up, a switch of mode)
//! waits for it. `sync/` hands it in; the cycle never sees what it is a lease on.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::RwLock;

/// Gives out the lease ([`Lease::hold`]).
#[derive(Clone)]
pub struct Lease(Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Held> + Send>> + Send + Sync>);

/// The lease while it is held: let go of when dropped, on whatever thread that is.
pub struct Held(#[allow(dead_code)] Box<dyn Send>);

impl Lease {
    /// A lease on `lock`, held for reading.
    pub fn on<T: Send + Sync + 'static>(lock: &Arc<RwLock<T>>) -> Self {
        let lock = Arc::clone(lock);
        Self(Arc::new(move || {
            let lock = Arc::clone(&lock);
            Box::pin(async move { Held(Box::new(lock.read_owned().await)) })
        }))
    }

    /// Waits for the lease. The caller gives up when it is cancelled (`cancellable`): a
    /// change of the folder cancels the cycle before it waits for it.
    pub async fn hold(&self) -> Held {
        (self.0)().await
    }
}
