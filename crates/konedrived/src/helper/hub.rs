//! The one link to konedrive-helper, shared by every account (design §2.1–§2.4).
//!
//! The helper sends a uid's opens to that uid's newest connection only, so one daemon keeps
//! one link for all its accounts. [`HelperHub`] holds it, with the helper's socket and
//! `HelperState`; [`supervise`] keeps it connected and [`watch`] keeps `HelperState` current.
//! It knows nothing of accounts or folders: whom the link serves is [`Served`], which the
//! registry of the daemon's folders implements (`sync::registry`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::{mpsc, watch, Notify};

use super::status::{HelperState, HelperUnit, NotAsked};
use super::{Clearance, HelperLink, HydrateRequest, LinkCell};

/// The longest [`supervise`] ever waits between attempts.
pub const MAX_HELPER_BACKOFF: Duration = Duration::from_secs(30);

/// Whom the link serves: the daemon's folders, told as the link comes and goes.
#[async_trait::async_trait]
pub trait Served: Send + Sync {
    /// `HelperState` is `now`. Called with the hub's publishing held: it only writes it
    /// down.
    fn helper_state(&self, now: HelperState);
    /// A helper is connected, and the hub holds its link: every folder is brought up on
    /// it, before any open is served.
    async fn helper_back(&self);
    /// Answers the opens the helper sends over `link` until the connection ends.
    async fn serve(self: Arc<Self>, link: HelperLink, requests: mpsc::Receiver<HydrateRequest>);
    /// The connection dropped, and the hub holds no link.
    fn helper_lost(&self);
}

/// The link to the helper, where its socket is, and `HelperState`, for one daemon.
pub struct HelperHub {
    /// Replaceable, because the helper can go away and come back:
    /// [`supervise`] swaps it for `None` the moment the connection drops and
    /// back to a live link when it reconnects. Shared with every account's
    /// folder, whose sync reads it at every reconcile.
    link: LinkCell,
    /// Where the helper's socket is, for local rule: with no
    /// link, a punch first looks there to see whether a helper — and so a
    /// fanotify group that could hold a mark — exists at all. Set by
    /// [`supervise`] to the path it connects to.
    socket: Mutex<PathBuf>,
    /// What `HelperState` asks while there is no link (HS1). Starts as
    /// [`NotAsked`], which asks nothing: only `main` installs
    /// systemd, so no test reaches the system bus.
    unit: Mutex<Arc<dyn HelperUnit>>,
    /// Told whenever the link comes or goes, so [`watch`] asks again at once.
    changed: Arc<Notify>,
    /// `Accounts.HelperState`; whom the link serves is told every change.
    state: watch::Sender<HelperState>,
    /// Held while `HelperState` is published, so that a link published while
    /// systemd is asked wins.
    publishing: Mutex<()>,
    /// Whom the link serves. Weak: they hold the hub.
    served: Weak<dyn Served>,
}

impl HelperHub {
    /// A hub that holds `link` already (a test's), or none, for `served`.
    pub fn new(link: Option<HelperLink>, served: Weak<dyn Served>) -> Arc<Self> {
        let state = if link.is_some() { HelperState::Connected } else { HelperState::Unknown };
        Arc::new(Self {
            link: LinkCell::holding(link),
            socket: Mutex::new(PathBuf::from(konedrive_proto::SOCKET_PATH)),
            unit: Mutex::new(Arc::new(NotAsked)),
            changed: Arc::new(Notify::new()),
            state: watch::Sender::new(state),
            publishing: Mutex::new(()),
            served,
        })
    }

    /// The live helper link, if there is one right now.
    pub fn link(&self) -> Option<HelperLink> {
        self.link.get()
    }

    /// The cell the link is kept in, for what reads it at every use.
    pub fn link_cell(&self) -> LinkCell {
        self.link.clone()
    }

    /// Where the helper's socket is. Defaults to `konedrive_proto::SOCKET_PATH`.
    pub fn set_socket(&self, path: impl Into<PathBuf>) {
        *crate::panic::lock(&self.socket) = path.into();
    }

    /// What a punch goes by when nothing ties it to a link of its own
    /// (local rule, on [`Clearance`]): the live link if there
    /// is one, the helper's socket if not.
    pub fn clearance(&self) -> Clearance {
        match self.link() {
            Some(link) => Clearance::Link(link),
            None => Clearance::NoLink(crate::panic::lock(&self.socket).clone()),
        }
    }

    /// What `HelperState` asks while there is no link (HS1): `main`
    /// installs systemd ([`super::status::Systemd`]); a test, a fake.
    pub fn set_unit(&self, unit: Arc<dyn HelperUnit>) {
        *crate::panic::lock(&self.unit) = unit;
        self.changed.notify_one();
    }

    /// `HelperState` (HS1).
    pub fn state(&self) -> HelperState {
        *self.state.borrow()
    }

    /// `HelperState` as it changes (`Accounts`'s `PropertiesChanged`).
    pub fn subscribe(&self) -> watch::Receiver<HelperState> {
        self.state.subscribe()
    }

    /// Publishes a new helper link, or its loss — and so `HelperState`
    /// (HS1): `connected` at once, or, on a loss, `unknown` until [`watch`]
    /// has asked systemd. Every account sees the same.
    pub fn set_link(&self, link: Option<HelperLink>) {
        let now = if link.is_some() { HelperState::Connected } else { HelperState::Unknown };
        let _publishing = crate::panic::lock(&self.publishing);
        self.link.set(link);
        self.publish(now);
        drop(_publishing);
        self.changed.notify_one();
    }

    /// Works `HelperState` out again: `connected` while there is a link,
    /// else what systemd says of the unit. A link that came up while systemd
    /// was being asked wins.
    pub async fn check(&self) {
        if self.link().is_some() {
            let _publishing = crate::panic::lock(&self.publishing);
            self.publish(HelperState::Connected);
            return;
        }
        let unit = Arc::clone(&crate::panic::lock(&self.unit));
        let found = match unit.states().await {
            Some((load, active)) => super::status::state_of_unit(&load, &active),
            None => HelperState::Unknown,
        };
        let _publishing = crate::panic::lock(&self.publishing);
        if self.link().is_none() {
            self.publish(found);
        }
    }

    /// `HelperState` for the hub and whom it serves, with `publishing` held.
    fn publish(&self, now: HelperState) {
        self.state.send_if_modified(|state| std::mem::replace(state, now) != now);
        if let Some(served) = self.served.upgrade() {
            served.helper_state(now);
        }
    }
}

/// Keeps a helper link alive for the life of the daemon.
///
/// Connects, has every folder brought up on that link ([`Served::helper_back`]: one after
/// another, each re-registers its root, whose walk the helper performs, then recovers
/// it), serves hydration requests until the
/// connection drops, publishes the drop to every account at once — not once
/// the downloads under way have finished — and tries again after a backoff
/// that grows to a cap. Nothing reconnected before: when the helper went
/// away `serve_hydrations` simply returned, `RootState` stayed `ready` with
/// `LastError` empty, and every un-hydrated file in the folder read as zeros
/// with nothing saying so.
///
/// `backoff` is the first delay; each failure doubles it up to
/// [`MAX_HELPER_BACKOFF`]. A successful connection resets it. Ends when there is nobody
/// left to serve.
pub async fn supervise(hub: Arc<HelperHub>, socket_path: PathBuf, backoff: Duration) {
    let mut wait = backoff;
    loop {
        let Some(served) = hub.served.upgrade() else { return };
        match HelperLink::connect(&socket_path).await {
            Ok((link, requests)) => {
                wait = backoff;
                tracing::info!("connected to the konedrive helper at {}", socket_path.display());
                hub.set_socket(&socket_path);
                hub.set_link(Some(link.clone()));
                // Re-register every root before serving anything: a helper
                // that has just started has no marks at all, and a root's
                // own registration is what puts them back.
                served.helper_back().await;
                // A task of its own, and the end of the connection is
                // waited for on the link itself. Awaiting the serving here
                // waited for every fill still
                // running as well — a download of any length — and until
                // then the loss was not published, the dead link was still
                // handed out, and nothing reconnected. The fills already
                // running finish in that task, their `HydrateDone` going
                // nowhere; the per-inode locks keep each of them ahead of
                // any fill of the same file on the next connection.
                let serving = tokio::spawn(Arc::clone(&served).serve(link.clone(), requests));
                link.closed().await;
                tracing::error!("the konedrive helper connection dropped");
                hub.set_link(None);
                served.helper_lost();
                drop(serving);
            }
            Err(e) => {
                tracing::warn!(
                    "cannot connect to the konedrive helper at {}: {e}; retrying in {:?}",
                    socket_path.display(),
                    wait
                );
            }
        }
        drop(served);
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(MAX_HELPER_BACKOFF);
    }
}

/// Keeps `HelperState` current for the life of the daemon (HS1): worked out
/// again whenever the link comes or goes, and every
/// [`RECHECK`](super::status::RECHECK) while there is none — a helper installed,
/// started or failed meanwhile shows within that.
pub async fn watch(hub: Arc<HelperHub>) {
    watch_every(hub, super::status::RECHECK).await
}

/// [`watch`], asking systemd again every `every` while there is no link
/// (tests: well under a second).
pub async fn watch_every(hub: Arc<HelperHub>, every: Duration) {
    let changed = Arc::clone(&hub.changed);
    loop {
        hub.check().await;
        if hub.link().is_some() {
            changed.notified().await;
        } else {
            tokio::select! {
                () = changed.notified() => {}
                () = tokio::time::sleep(every) => {}
            }
        }
    }
}
