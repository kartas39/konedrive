//! What the sync tells the user about itself: the activity log and the
//! conflicts ([`Activity`]), the downloads under way ([`Transfers`]), and the
//! space the folder takes ([`LocalSpace`]).
//!
//! [`Report`] bundles them, so that everything that downloads, frees up or
//! reconciles — `SyncService`, the hydration loop, a OneDrive folder's
//! listing and its replacements — reports into the same places, and
//! `dbus::signals` publishes from there.

use std::sync::Arc;

use crate::status::activity::Activity;
use crate::status::snapshot::{SyncSnapshot, SyncStateHandle};
use crate::status::space::LocalSpace;
use crate::status::transfers::Transfers;

/// Everything the sync reports into (see the module's doc comment). Cheap to
/// clone; every clone reports into the same places.
#[derive(Clone)]
pub struct Report {
    pub activity: Arc<Activity>,
    pub transfers: Transfers,
    pub space: Arc<LocalSpace>,
}

impl Report {
    pub fn new(state: SyncStateHandle) -> Self {
        Self {
            activity: Arc::new(Activity::new(state.clone())),
            transfers: Transfers::default(),
            space: Arc::new(LocalSpace::new(state)),
        }
    }

    /// A report that goes nowhere: the plain `serve_hydrations`', which no
    /// service reads. It records into a log nobody reads and starts no
    /// walker.
    pub fn nowhere() -> Self {
        let state = SyncStateHandle::new(SyncSnapshot::default());
        Self {
            activity: Arc::new(Activity::new(state.clone())),
            transfers: Transfers::default(),
            space: Arc::new(LocalSpace::inert(state)),
        }
    }
}
