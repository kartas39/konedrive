//! The sync folder on disk: its registration ([`root`]), the names the daemon uses for itself
//! in it and the lock on its directories' modes ([`disk`]), and the walk over its files
//! ([`walk`]).

pub(crate) mod disk;
pub(crate) mod root;
pub(crate) mod walk;
