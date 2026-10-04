//! Sign-in with Microsoft, and everything that talks to Microsoft Graph.
//!
//! What the crate offers, by module:
//!
//! - The sign-in, in [`oauth`], [`pkce`] and [`loopback`]: the authorization URL and the
//!   token endpoint, PKCE, the listener for the redirect.
//! - Access tokens for Graph callers, in [`token`], and the store the refresh token is
//!   kept in, in [`secret`].
//! - The drive API, in [`drive`]: [`drive::DriveClient`], its items, its errors, the
//!   change notifications.
//! - The transfer pool, in [`pool`]: how many requests an account has in flight.
//! - QuickXorHash, in [`quickxor`].
//!
//! What only tests need (a secret store in memory, a pool that starts at a given size) is
//! built with the feature `testing`, and in this crate's own tests.

pub mod drive;
pub mod loopback;
pub mod oauth;
pub mod pkce;
pub mod pool;
pub mod quickxor;
pub mod secret;
pub mod token;

/// `mutex`, locked; also after a holder of it panicked. What is done under a lock of this
/// crate is one assignment or one reading, so the data is whole either way.
pub(crate) fn lock<T: ?Sized>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
