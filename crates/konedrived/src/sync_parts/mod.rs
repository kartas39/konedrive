//! What the tasks of one folder's sync share: the steps that run now ([`running_steps`]), the
//! wake of the plan ([`sync_plan_wake`]), the plan's answers as last published
//! ([`published_plan`]), what a step answers ([`step_outcome`]), what holds sending
//! ([`sending_holds`]), whether OneDrive has space ([`onedrive_space`]) and the folder's lease
//! ([`lease`]).

// No caller until the owner of a folder's state starts a sync through `sync_tasks`.
pub mod lease;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
pub mod onedrive_space;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
pub mod published_plan;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
pub mod running_steps;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
pub mod sending_holds;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
pub mod step_outcome;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
pub mod sync_plan_wake;
