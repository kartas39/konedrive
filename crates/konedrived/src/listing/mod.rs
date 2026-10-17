//! The listing of OneDrive into the rows: a cycle of the delta listing ([`listing_cycle`]),
//! the poller that runs it on its schedule ([`listing_poller`]) with the notification socket
//! beside it ([`live`]), and what a listed item is in the store ([`classify`]).

pub(crate) mod classify;
pub(crate) mod listing_cycle;
pub(crate) mod listing_poller;
pub(crate) mod live;
