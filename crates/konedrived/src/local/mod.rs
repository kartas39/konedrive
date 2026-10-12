//! The names of a folder that stay local ([`ignore`]) and the ones OneDrive refuses
//! ([`names`]).

mod ignore;
pub(crate) mod names;

pub(crate) use ignore::{IgnoreList, SharedIgnore};
