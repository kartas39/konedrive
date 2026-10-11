//! The runners of steps as the plan's task takes them: one seam, behind which are the
//! three real runners or, in the task's own tests, a stand-in.
//!
//! **State owned:** none: two functions held as values.
//!
//! **Answers for:** the task being filled and tested before the runners are, and never
//! naming them: [`StepRunners::of_runners`] is the one place that knows which runner a
//! step goes to and under what (the folder's lease for a step towards the folder).
//!
//! **Tested without the rest:**
// test: each_case_of_a_step_goes_to_its_runner
// test: a_step_towards_the_folder_runs_under_the_folders_lease_and_a_download_does_not
// test: the_show_steps_go_to_the_folder_steps_as_one_call
#![allow(dead_code, unused_variables)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use konedrive_tree::{FolderStep, ItemVersions, SyncStep};

use super::row_steps::RowSteps;
use crate::folder::step_outcome::StepOutcome;
use crate::fuse::FolderSteps;
use crate::remote::listing::Lease;
use crate::upload::onedrive_steps::OneDriveSteps;

/// A step being run.
pub type RunningFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// What runs the steps of one folder's plan.
#[derive(Clone)]
pub struct StepRunners {
    /// Runs one step, any case, and answers what came of it.
    pub run_step: Arc<dyn Fn(SyncStep) -> RunningFuture<StepOutcome> + Send + Sync>,
    /// Runs the `ShowItem` steps of one plan together, each answered by itself, in the
    /// order given.
    pub run_show_steps: Arc<dyn Fn(Vec<ItemVersions>) -> RunningFuture<Vec<StepOutcome>> + Send + Sync>,
}

impl StepRunners {
    /// The three runners: a step of the rows to `RowSteps::run`; a step towards OneDrive
    /// to `OneDriveSteps::run`; `DownloadContent` to `FolderSteps::run_download_content`,
    /// with no lease; every other step towards the folder to `FolderSteps::run`, and the
    /// show steps to `FolderSteps::run_show_items`, each under the folder's lease, so that
    /// a Forget waits for it as it waits for a reconcile today. No slot of the pool is
    /// taken here: an upload's by `OneDriveSteps`' content upload, a download's by the
    /// downloader. Made by the folder's start (`sync_tasks::start_order`).
    pub fn of_runners(folder_steps: Arc<FolderSteps>, onedrive_steps: Arc<OneDriveSteps>, row_steps: Arc<RowSteps>, lease: Lease) -> Self {
        let for_show_steps = (Arc::clone(&folder_steps), lease.clone());
        StepRunners {
            run_step: Arc::new(move |step| {
                let (folder_steps, onedrive_steps, row_steps, lease) = (Arc::clone(&folder_steps), Arc::clone(&onedrive_steps), Arc::clone(&row_steps), lease.clone());
                Box::pin(async move {
                    match step {
                        SyncStep::Row(step) => row_steps.run(step).await,
                        SyncStep::OneDrive(step) => onedrive_steps.run(step).await,
                        SyncStep::Folder(FolderStep::DownloadContent { item }) => folder_steps.run_download_content(item).await,
                        SyncStep::Folder(step) => {
                            let _held = lease.hold().await;
                            folder_steps.run(step).await
                        }
                    }
                })
            }),
            run_show_steps: Arc::new(move |items| {
                let (folder_steps, lease) = (Arc::clone(&for_show_steps.0), for_show_steps.1.clone());
                Box::pin(async move {
                    let _held = lease.hold().await;
                    folder_steps.run_show_items(items).await
                })
            }),
        }
    }
}
