//! What the tasks of one folder's sync share: the steps that run now ([`running_steps`]), the
//! wake of the plan ([`sync_plan_wake`]), the plan's answers as last published
//! ([`published_plan`]), what a step answers ([`step_outcome`]), what holds sending
//! ([`sending_holds`]), whether OneDrive has space ([`onedrive_space`]) and the folder's lease
//! ([`lease`]).

pub(crate) mod lease;
pub(crate) mod onedrive_space;
pub(crate) mod published_plan;
pub(crate) mod running_steps;
pub(crate) mod sending_holds;
pub(crate) mod step_outcome;
pub(crate) mod sync_plan_wake;
