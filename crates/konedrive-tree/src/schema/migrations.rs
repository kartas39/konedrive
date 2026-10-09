//! The steps that bring a store of an earlier version to today's schema, in
//! order. Each starts from one version and ends at the next, and runs with
//! the change of the version in one transaction (`at_version`), so a store
//! is never left between two.
//!
//! A step is written once and not changed afterwards: its SQL is its own,
//! not the constants the rest of the crate uses today, so that a later
//! change of the schema is a new step and never a different old one.
//!
//! There is none yet: the schema has had one version.

use rusqlite::Connection;

use crate::TreeError;

pub(super) struct Step {
    from: &'static str,
    pub(super) to: &'static str,
    /// For the journal: what the step did.
    pub(super) what: &'static str,
    pub(super) run: fn(&Connection) -> Result<(), TreeError>,
}

const STEPS: [Step; 0] = [];

/// The step that starts from `version`.
pub(super) fn from(version: Option<&str>) -> Option<&'static Step> {
    STEPS.iter().find(|step| Some(step.from) == version)
}
