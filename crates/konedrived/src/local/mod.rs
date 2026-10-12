//! The names of a folder that stay local ([`ignore`]) and the ones OneDrive refuses
//! ([`names`]).

mod ignore;
pub mod names;

pub use ignore::{IgnoreList, SharedIgnore, DEFAULT_PATTERNS};
