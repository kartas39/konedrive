//! The daemon's side of the helper: the link to it ([`HelperLink`], `link`), the one link
//! all accounts share and who is served by it (`hub`), what a move out asks of it
//! (`linked`), whether it runs (`presence`, `status`), and the rule every punch clears its
//! way by (`clearance`).

mod clearance;
pub mod hub;
mod link;
pub mod linked;
mod presence;
pub mod status;
#[cfg(test)]
pub(crate) mod testing;

pub use clearance::{Clearance, NotCleared};
pub use link::{reopen_for_writing, HelperLink, HydrateRequest, LinkCell};
pub use presence::{helper_presence, HelperPresence};

#[derive(Debug, thiserror::Error)]
pub enum HelperError {
    #[error("the konedrive helper is not running")]
    NotRunning,
    #[error("the helper refused the request (errno {0})")]
    Refused(i32),
    #[error("the helper did not respond within its call timeout")]
    Timeout,
    #[error("{0}")]
    Io(String),
}
