//! One cycle of OneDrive's delta listing written into the rows: the pages fetched with
//! today's Graph code and given to the store page by page.
//!
//! **State owned:** the turn (one cycle at a time). Which listing is under way, the link
//! to its next page, the delta link and the count the guard against an outdated listing
//! compares with are the store's (`item_store::onedrive_versions`); a full listing asked
//! for is the store's too (`full_listing_request`). Nothing of a cycle is kept in memory
//! between two cycles: a stop loses nothing.
//!
//! **Answers for:** design §4 — the cycle writes OneDrive versions only, and takes no
//! tree lock and no lease of the folder: a `mkdir` does not wait behind a listing — and
//! design §7's guard: a page is given with the count read when its fetch began; what the
//! store answers is outdated is read again and given again with the same link, and no
//! link is asked from before a call has written it. It replaces `listing/fetch.rs`,
//! `listing/stage.rs` and `listing/reconcile.rs`; what a cycle decided for the folder is
//! the plan's now, which is woken after every page.
//!
//! **One cycle:**
//!
//! 1. the account is checked against the folder's drive (`check_account`, as today);
//! 2. how to begin is decided ([`decide_listing_start`]) and said to the store
//!    (`begin_full_listing`, `begin_delta_listing`, or the full listing found under way
//!    with the next link it kept). One listing id names one enumeration of OneDrive:
//!    what is asked from the start is always a listing begun anew;
//! 3. per page: the count read (`ItemStore::confirmation_count`), the page fetched
//!    (`DriveClient::delta`), classified (`folder::classify`), written
//!    (`write_listed_changes`); while the answer names items to read again, each is read
//!    (`DriveClient::item`), given back under the id that was asked, and the call
//!    repeated with the same link; the plan woken;
//! 4. a refused link is decided ([`decide_page_refusal`]): 410 and a refused resume link
//!    begin a full listing in the same cycle; a delta link refused otherwise fails the
//!    cycle and is kept;
//! 5. the page with the delta link ends the listing: the plan woken with `ListingEnded`,
//!    the thumbnail filler nudged, `last_checked` written. The folder's counts on the bus
//!    are not the cycle's: the plan's task, woken after every page, is their one writer.
//!
//! A page the store refuses because its listing is not the one under way any more is
//! dropped, and the cycle ends there with no fault ([`is_listing_given_up`]). A full
//! listing is begun for a refused link once in a cycle ([`FullListingAllowance`]), and
//! the items of one page are read again [`READ_AGAIN_ROUNDS`] times at most: beyond
//! either the cycle fails, and the next one decides anew from what the store kept — the
//! next link of a full listing, or else a listing begun anew.
//!
//! **Tested without the rest**, against the fake OneDrive and the fixture's store
//! (`mount/item_fixture.rs`), with no mount and nothing of the folder but its rows:
// test: a_store_with_no_delta_link_begins_a_full_listing
// test: a_store_with_a_delta_link_lists_the_changes_since_it
// test: a_full_listing_that_was_stopped_goes_on_from_the_link_the_store_kept
// test: a_listing_of_changes_that_was_stopped_is_asked_again_from_the_delta_link
// test: a_full_listing_asked_for_is_begun_though_the_store_has_a_delta_link
// test: each_page_is_written_with_the_count_read_before_its_fetch
// test: an_item_the_store_answers_is_outdated_is_read_again_and_given_with_the_same_link
// test: an_outdated_item_that_is_gone_in_onedrive_is_given_back_as_gone
// test: no_next_page_is_asked_before_the_link_was_written
// test: a_410_begins_a_full_listing_in_the_same_cycle
// test: a_resume_link_onedrive_refuses_begins_a_full_listing
// test: a_delta_link_onedrive_refuses_fails_the_cycle_and_is_kept
// test: a_next_link_of_this_cycle_that_onedrive_refuses_fails_the_cycle_and_is_resumed_from
// test: a_cycle_cancelled_between_two_pages_leaves_the_link_of_the_last_page_written
// test: the_plan_is_woken_after_every_page_and_when_the_listing_ends
// test: the_cycle_takes_no_tree_lock
// test: a_page_of_a_listing_the_store_gave_up_is_dropped_without_a_fault
// test: a_page_the_store_refuses_of_the_listing_under_way_fails_the_cycle
// test: a_listing_is_given_up_when_another_or_none_is_under_way
// test: two_cycles_never_run_at_once
// test: a_drive_that_is_not_the_folders_stops_the_cycle_before_anything_is_listed
// test: a_full_listing_stopped_before_its_first_link_was_written_is_begun_anew
// test: a_second_410_in_one_cycle_fails_the_cycle_and_the_next_begins_a_full_listing_anew
// test: items_that_stay_outdated_are_read_again_so_many_rounds_and_then_the_cycle_fails
// test: an_item_read_again_is_given_back_under_the_id_that_was_asked
// test: a_drive_another_account_has_recorded_is_not_listed_into_this_folder
// test: a_drive_is_the_same_however_its_id_is_spelled
// test: the_first_cycle_records_the_accounts_drive_in_config_toml

use std::sync::Arc;

use konedrive_graph::drive::{DeltaFrom, DeltaNext, DriveClient, DriveError};
use konedrive_tree::{Change, ListedChangeOutcome, ListedPageOutcome, ListingId, ListingKind, ListingLink, ListingUnderWay, OneDriveItemId, Store, TreeError};
use tokio_util::sync::CancellationToken;

use crate::listing::classify::classify;
use crate::sync_parts::sync_plan_wake::{SyncPlanWake, WakeCause};
use crate::status::report::Report;
use crate::status::snapshot::{SyncStateHandle, SyncTrouble, TroubleKind};

/// How a cycle begins, and what a refused page leads to.
mod listing_start;

pub(crate) use listing_start::{decide_listing_start, decide_page_refusal, AfterPageRefusal, FullListingAllowance, ListingStart, PageLinkOrigin};

/// One page as it was fetched: what it says, what followed it, and the count of
/// confirmations read before the request left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchedPage {
    pub(crate) changes: Vec<Change>,
    pub(crate) link: ListingLink,
    pub(crate) fetch_began_at_count: u64,
}

/// Why a page was not fetched.
#[derive(Debug)]
enum PageFetchError {
    /// The cycle was cancelled while the request was under way; nothing was written.
    Cancelled,
    /// The count of confirmations could not be read; no request left.
    Store(TreeError),
    /// What Graph answered the request with: [`decide_page_refusal`] says what follows.
    Graph(DriveError),
}

/// What became of a page given to the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageWrite {
    /// Its link is written; so many items were read again on the way.
    LinkWritten { read_again: usize },
    /// The store gave the page's listing up for a later one, or it ended: the page is
    /// dropped, and nothing of it was written.
    ListingGivenUp,
}

/// How many times in a row the items of one page are read again before the cycle fails:
/// each round needs a step that confirmed something of them while they were read, and the
/// next cycle asks the page again from the link the store kept.
const READ_AGAIN_ROUNDS: usize = 5;

/// Whether the store, which refused a page of `listing`, did so because that listing is
/// not the one under way any more (`under_way`, read in the job that was refused): the
/// page is dropped then, and any other refusal is a fault of the cycle. Pure.
fn is_listing_given_up(listing: ListingId, under_way: Option<ListingId>) -> bool {
    under_way != Some(listing)
}

/// `read`, what OneDrive answered the reading of the item `asked`, as a change of that
/// item: under the id that was asked, however the answer spells it. The store finds a
/// row by its item id letter for letter, and answers again an item it was not given back:
/// under another spelling the row asked for would stay unanswered, and a full listing
/// that waits for it would not end. Pure.
fn name_by_asked_id(read: Change, asked: &OneDriveItemId) -> Change {
    let name = |mut row: konedrive_tree::Row| {
        row.id = asked.as_str().to_owned();
        row
    };
    match read {
        Change::Root(row) => Change::Root(name(row)),
        Change::Upsert(row) => Change::Upsert(name(row)),
        Change::Delete(_) => Change::Delete(asked.as_str().to_owned()),
    }
}

/// `request`, unless `cancel` fires first; it is dropped then. Only for a request to
/// Graph and the wait for the turn: nothing is written until either answers. Never for a
/// job of the store, which is what makes a page being written be written whole.
async fn cancellable<T>(cancel: &CancellationToken, request: impl std::future::Future<Output = T>) -> Result<T, CycleError> {
    cancel.run_until_cancelled(request).await.ok_or(CycleError::Cancelled)
}

/// Trouble with Graph as a cycle says it, as today (`listing.rs:185`).
fn describe_drive_error(error: DriveError) -> CycleError {
    match error {
        DriveError::SignedOut => CycleError::SignedOut,
        other => CycleError::Offline(other.to_string()),
    }
}

/// Says no full listing runs when the cycle ends, however it ends.
struct FullListingShown<'a>(&'a SyncStateHandle);

impl Drop for FullListingShown<'_> {
    fn drop(&mut self) {
        self.0.update_if_changed(|state| state.cycle.listing = false);
    }
}

/// What one cycle did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ListingCycleReport {
    pub(crate) kind: ListingKind,
    pub(crate) pages: usize,
    /// How many changes the pages named.
    pub(crate) changes: usize,
    /// How many items were read again for the guard against an outdated listing.
    pub(crate) read_again: usize,
}

/// The listing of one folder.
pub(crate) struct ListingCycle {
    drive: DriveClient,
    store: Store,
    sync_plan_wake: Arc<SyncPlanWake>,
    state: SyncStateHandle,
    /// The account's drive as `config.toml` keeps it; `None` in tests.
    drive_record: Option<DriveRecord>,
    neighbours: Option<Neighbours>,
    /// Where the conflicts on record are looked over at a listing's end.
    report: Report,
    /// Nudged at a listing's end: the thumbnail filler's.
    after_listing: Option<Arc<tokio::sync::Notify>>,
    /// One cycle at a time.
    turn: tokio::sync::Mutex<()>,
}

/// What a listing cycle is made with.
pub(crate) struct ListingCycleSetup {
    pub(crate) drive: DriveClient,
    pub(crate) store: Store,
    pub(crate) sync_plan_wake: Arc<SyncPlanWake>,
    pub(crate) state: SyncStateHandle,
    pub(crate) drive_record: Option<DriveRecord>,
    pub(crate) neighbours: Option<Neighbours>,
    pub(crate) report: Report,
    pub(crate) after_listing: Option<Arc<tokio::sync::Notify>>,
}

impl ListingCycle {
    /// Made by the folder's start (`sync_tasks::start_order::start_sync_tasks`) and
    /// given to its poller (`listing_poller::ListingPoller::start`).
    pub fn new(setup: ListingCycleSetup) -> Arc<Self> {
        let ListingCycleSetup { drive, store, sync_plan_wake, state, drive_record, neighbours, report, after_listing } = setup;
        Arc::new(ListingCycle { drive, store, sync_plan_wake, state, drive_record, neighbours, report, after_listing, turn: tokio::sync::Mutex::new(()) })
    }

    /// The account's drive, for the live task the poller starts beside the cycle.
    pub(super) fn drive(&self) -> &DriveClient {
        &self.drive
    }

    /// The folder's store, for the live task.
    pub(super) fn store(&self) -> &Store {
        &self.store
    }

    /// The folder's published state, for the live task and the poller's stop.
    pub(super) fn state(&self) -> &SyncStateHandle {
        &self.state
    }

    /// One cycle, from the check of the account to the listing's end or the first
    /// failure, and what came of it published (the folder's trouble, `LastChecked`).
    /// Called by `ListingPoller`, on its schedule, on a refresh and when the live socket
    /// says so, as today's `Listing::cycle` (`listing.rs:284`). `cancel` ends it between two
    /// requests; the page being written is written whole. A page the store refuses because
    /// its listing was given up ends the cycle with what it did until then, and no fault.
    pub(crate) async fn run(&self, cancel: &CancellationToken) -> Result<ListingCycleReport, CycleError> {
        let result = self.run_in_turn(cancel).await;
        self.state.update(|state| match &result {
            Ok(_) => state.cycle.sync_trouble = None,
            Err(CycleError::Cancelled) => {}
            Err(error) => state.cycle.sync_trouble = Some(SyncTrouble { text: error.to_string(), blocking: error.blocking(), kind: error.kind() }),
        });
        result
    }

    /// [`run`](Self::run) without what it publishes: the turn taken, then the cycle.
    async fn run_in_turn(&self, cancel: &CancellationToken) -> Result<ListingCycleReport, CycleError> {
        let _turn = cancellable(cancel, self.turn.lock()).await?;
        self.check_account(cancel).await?;
        let (mut listing, mut from, mut origin) = self.begin(false).await?;
        let _shown = FullListingShown(&self.state);
        let mut report = ListingCycleReport { kind: listing.kind, pages: 0, changes: 0, read_again: 0 };
        let mut allowance = FullListingAllowance::Unused;
        loop {
            self.state.update_if_changed(|state| state.cycle.listing = listing.kind == ListingKind::Full);
            let page = match self.fetch_page(&from, cancel).await {
                Ok(page) => page,
                Err(PageFetchError::Cancelled) => return Err(CycleError::Cancelled),
                Err(PageFetchError::Store(error)) => return Err(error.into()),
                Err(PageFetchError::Graph(error)) => match decide_page_refusal(&error, origin, allowance) {
                    AfterPageRefusal::BeginFullListing => {
                        tracing::info!("OneDrive would not go on with the listing ({error}); listing the drive again from the start");
                        allowance = FullListingAllowance::Used;
                        (listing, from, origin) = self.begin(true).await?;
                        report.kind = listing.kind;
                        continue;
                    }
                    AfterPageRefusal::FailCycle => return Err(describe_drive_error(error)),
                },
            };
            let (change_count, link) = (page.changes.len(), page.link.clone());
            match self.write_page(listing.id, page, cancel).await? {
                PageWrite::LinkWritten { read_again } => report.read_again += read_again,
                PageWrite::ListingGivenUp => {
                    tracing::info!("the listing {} was given up for a later one; its page is dropped", listing.id);
                    return Ok(report);
                }
            }
            report.pages += 1;
            report.changes += change_count;
            match link {
                ListingLink::NextLink(next) => {
                    (from, origin) = (DeltaFrom::Link(next), PageLinkOrigin::GivenInThisCycle);
                }
                // The listing ended in the store with this page: that is told before
                // anything that may fail is read.
                ListingLink::DeltaLink(_) => {
                    self.end_cycle(&report).await?;
                    return Ok(report);
                }
            }
        }
    }

    /// The drive the token reaches is the folder's: as today (`listing.rs:395-428`), with
    /// the drive written to `config.toml` here, at the first cycle that sees it, where
    /// today the next reconcile writes it under the folder's lock (`reconcile.rs:218`;
    /// the map's MISSING-4.a): the write is of the account's record alone and needs no
    /// lock of the folder. Also sets the store's drive, which `write_listed_changes`
    /// refuses to work without. Nothing is kept here of a drive written: whether it is
    /// asked of `config.toml` itself, so the write is done by one cycle. The mark of the
    /// drive on the folder's own directory (`folder::root::mark_drive`) is not written
    /// here: the cycle does not know the folder's path.
    async fn check_account(&self, cancel: &CancellationToken) -> Result<(), CycleError> {
        let id = cancellable(cancel, self.drive.drive_id()).await?.map_err(describe_drive_error)?;
        let stored = self.store.call(|store| store.drive_id()).await?;
        let kept = self.drive_record.as_ref().and_then(|record| record.recorded.clone());
        // Compared as drive ids: one drive is the same however its id is spelled.
        let is_same_drive = |recorded: &String| crate::config::DriveId::new(recorded.as_str()) == crate::config::DriveId::new(id.as_str());
        if let Some(recorded) = stored.clone().or(kept.clone()).filter(|recorded| !is_same_drive(recorded)) {
            // The account learns which drive its token reaches now.
            if let Some(neighbours) = &self.neighbours {
                (neighbours.drive_seen)(&id);
            }
            return Err(CycleError::OtherAccount(recorded));
        }
        // A drive is one account: one another account has recorded is not listed a second
        // time into this folder.
        let record = self.drive_record.as_ref().filter(|_| kept.is_none());
        if let Some(record) = record {
            let config = record.store.snapshot();
            if let Some(other) = config.accounts.iter().find(|account| account.id != record.account && account.drive_id.as_ref().is_some_and(|drive| *drive == id)) {
                return Err(CycleError::DriveTaken(other.label.clone()));
            }
        }
        if stored.is_none() {
            let recorded = id.clone();
            self.store.call(move |store| store.set_drive_id(&recorded)).await?;
        }
        if let Some(record) = record.filter(|record| record.store.account(&record.account).is_some_and(|account| account.drive_id.is_none())) {
            if let Some(drive) = crate::config::DriveId::new(id.as_str()) {
                let record = record.clone();
                let written = tokio::task::spawn_blocking(move || record.store.record_drive(&record.account, &drive)).await;
                match written {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => tracing::warn!("cannot record the account's drive in config.toml: {error}"),
                    Err(error) => tracing::warn!("the task recording the account's drive failed: {error}"),
                }
            }
        }
        Ok(())
    }

    /// Reads from the store what [`decide_listing_start`] asks — the listing under way,
    /// the delta link, whether a full listing was asked for — in one job, decides, and
    /// says the beginning to the store: `begin_full_listing`, `begin_delta_listing`, or
    /// nothing for a full listing that goes on from its next link. Answers the listing
    /// and where its first request goes: from the start only for a listing begun here,
    /// under an id no page was written with. `start_over`: OneDrive refused the link the
    /// cycle began with, and a full listing begins whatever the store says.
    async fn begin(&self, start_over: bool) -> Result<(ListingUnderWay, DeltaFrom, PageLinkOrigin), CycleError> {
        let begun = self
            .store
            .call(move |store| {
                let start = match start_over {
                    true => ListingStart::BeginFullListing,
                    false => {
                        let (under_way, is_full_listing_requested) = store.read_items(|reader| Ok((reader.find_listing_under_way()?, reader.is_full_listing_requested()?)))?;
                        decide_listing_start(under_way.as_ref(), store.delta_link()?.as_deref(), is_full_listing_requested)
                    }
                };
                Ok(match start {
                    ListingStart::GoOnWithFullListing { listing, next_link } => (listing, DeltaFrom::Link(next_link), PageLinkOrigin::KeptNextLink),
                    ListingStart::BeginFullListing => (store.item_store().begin_full_listing()?, DeltaFrom::Start, PageLinkOrigin::GivenInThisCycle),
                    ListingStart::BeginDeltaListing { delta_link } => (store.item_store().begin_delta_listing()?, DeltaFrom::Link(delta_link), PageLinkOrigin::KeptDeltaLink),
                })
            })
            .await?;
        Ok(begun)
    }

    /// Reads the count of confirmations (`ItemStore::confirmation_count`), then asks Graph
    /// for the page at `from` (`DriveClient::delta`,
    /// `crates/konedrive-graph/src/drive/mod.rs:199`) and classifies its items
    /// (`folder::classify::classify`, unchanged). The count is read first: what a step
    /// confirms while the request is under way is stamped later than the page.
    async fn fetch_page(&self, from: &DeltaFrom, cancel: &CancellationToken) -> Result<FetchedPage, PageFetchError> {
        let fetch_began_at_count = self.read_confirmation_count().await.map_err(PageFetchError::Store)?;
        let page = cancel.run_until_cancelled(self.drive.delta(from)).await.ok_or(PageFetchError::Cancelled)?.map_err(PageFetchError::Graph)?;
        let link = match page.next {
            DeltaNext::Page(next) => ListingLink::NextLink(next),
            DeltaNext::Done(delta_link) => ListingLink::DeltaLink(delta_link),
        };
        Ok(FetchedPage { changes: page.items.iter().map(classify).collect(), link, fetch_began_at_count })
    }

    /// The count of confirmations as the store's thread has it now
    /// (`ItemStore::confirmation_count`): read there, behind every write given before, so
    /// that it is never before the count a listing just begun began at.
    async fn read_confirmation_count(&self) -> Result<u64, TreeError> {
        self.store.call(|store| store.item_store().confirmation_count()).await
    }

    /// Writes `page` (`ItemStore::write_listed_changes`), and while the answer names items
    /// to read again, reads them ([`read_items_again`](Self::read_items_again)) and calls
    /// again with what was read, the count that reading began at, and the same link —
    /// until the link is written. Wakes the plan after each call that wrote rows. Answers
    /// how many items were read again, or that the store gave the listing up
    /// ([`is_listing_given_up`]): the page is dropped then, with no fault. The store is
    /// one job per call: no lock of the folder is held.
    async fn write_page(&self, listing: ListingId, page: FetchedPage, cancel: &CancellationToken) -> Result<PageWrite, CycleError> {
        let FetchedPage { mut changes, link, mut fetch_began_at_count } = page;
        let mut read_again = 0;
        for round in 0..=READ_AGAIN_ROUNDS {
            let (given, link_given) = (std::mem::take(&mut changes), link.clone());
            let written: Option<ListedPageOutcome> = self
                .store
                .call(move |store| match store.item_store().write_listed_changes(listing, &given, fetch_began_at_count, &link_given) {
                    Ok(outcome) => Ok(Some(outcome)),
                    Err(TreeError::Refused(refusal)) => match is_listing_given_up(listing, store.item_store().find_listing_under_way()?.map(|under_way| under_way.id)) {
                        true => Ok(None),
                        false => Err(TreeError::Refused(refusal)),
                    },
                    Err(error) => Err(error),
                })
                .await?;
            let Some(outcome) = written else { return Ok(PageWrite::ListingGivenUp) };
            if outcome.is_link_written() || outcome.changes.contains(&ListedChangeOutcome::Written) {
                self.sync_plan_wake.wake(WakeCause::ListingPageWritten);
            }
            if outcome.is_link_written() {
                return Ok(PageWrite::LinkWritten { read_again });
            }
            if round == READ_AGAIN_ROUNDS {
                break;
            }
            read_again += outcome.outdated_items.len();
            (changes, fetch_began_at_count) = self.read_items_again(&outcome.outdated_items, cancel).await?;
        }
        Err(CycleError::Apply(format!("what OneDrive has of some items changed {READ_AGAIN_ROUNDS} times while they were read again")))
    }

    /// Each of `items` as OneDrive has it now: an upsert, or a removal for one that is
    /// gone (`DriveClient::item`, `crates/konedrive-graph/src/drive/mod.rs:217`; today's
    /// `fresh`, `listing/stage.rs:182`), and the count read before the first request,
    /// which the next call of `write_listed_changes` is given with them and the page's
    /// own link. Every item asked is answered, as there or as gone, and under the id
    /// that was asked ([`name_by_asked_id`]): the store answers again one that is not.
    async fn read_items_again(&self, items: &[OneDriveItemId], cancel: &CancellationToken) -> Result<(Vec<Change>, u64), CycleError> {
        let read_began_at_count = self.read_confirmation_count().await?;
        let mut changes = Vec::with_capacity(items.len());
        for item in items {
            changes.push(match cancellable(cancel, self.drive.item(item.as_str())).await? {
                Ok(read) => name_by_asked_id(classify(&read), item),
                Err(DriveError::NotFound) => Change::Delete(item.as_str().to_owned()),
                Err(error) => return Err(describe_drive_error(error)),
            });
        }
        Ok((changes, read_began_at_count))
    }

    /// The listing ended: the plan woken with `WakeCause::ListingEnded`, the thumbnail
    /// filler nudged (`after_listing`), `last_checked` written
    /// (`TreeStore::set_last_checked`), the conflicts on record looked over (through
    /// `report`). As today's `after_cycle` (`listing.rs:340`) without what told the
    /// upload queue. Called as soon as the store ended the listing.
    /// The wake and the nudge come first, and nothing that can fail stands before them:
    /// the listing ended in the store whatever becomes of the rest.
    async fn end_cycle(&self, report: &ListingCycleReport) -> Result<(), CycleError> {
        self.sync_plan_wake.wake(WakeCause::ListingEnded);
        if let Some(after_listing) = &self.after_listing {
            after_listing.notify_one();
        }
        self.state.update_if_changed(|state| state.cycle.listing = false);
        let now = crate::clock::unix_now();
        self.store.call(move |store| store.set_last_checked(now)).await?;
        self.state.update(|state| state.cycle.last_checked = now);
        // A conflict whose kept copy is gone drops off by itself, a batch of them
        // looked over at each listing's end. The activity log asks the store itself.
        let activity = Arc::clone(&self.report.activity);
        if let Err(error) = tokio::task::spawn_blocking(move || activity.prune()).await {
            tracing::warn!("the task looking over the conflicts failed: {error}");
        }
        tracing::debug!("a {:?} listing ended: {} pages, {} changes, {} items read again", report.kind, report.pages, report.changes, report.read_again);
        Ok(())
    }
}

#[cfg(test)]
pub(super) mod tests;

/// The account's drive, as `config.toml` keeps it (`docs/design/accounts.md` §6.1): the
/// same-account check then survives a tree store rebuilt empty, whose `meta`
/// has forgotten it.
#[derive(Clone)]
pub(crate) struct DriveRecord {
    pub(crate) store: Arc<crate::config::ConfigStore>,
    /// The account's id.
    pub(crate) account: crate::config::AccountId,
    /// What `config.toml` recorded when the sync started; `None` when
    /// nothing was, and the first cycle writes it.
    pub(crate) recorded: Option<String>,
}

/// What a cycle asks of, or tells, the rest of the daemon.
#[derive(Clone)]
pub(crate) struct Neighbours {
    /// The drive the account's token reaches, when a cycle finds it is not
    /// the folder's: the account records it and works its
    /// mode out again, so that a read-write account turns read-only.
    pub(crate) drive_seen: DriveSeen,
}

/// Told the drive an account's token reaches.
pub(crate) type DriveSeen = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum CycleError {
    #[error("signed out: sign in again to keep this folder in step with OneDrive")]
    SignedOut,
    #[error(
        "this folder was listed from another OneDrive account (drive {0}); forget it and register \
         a folder for the account signed in now"
    )]
    OtherAccount(String),
    /// The account is signed in to a drive another account of this daemon has
    /// (`docs/design/accounts.md` §6.1); that account's label. Two folders of one drive would
    /// download everything twice.
    #[error(
        "this account is signed in to the Microsoft account already connected as '{0}'; sign it \
         out, or remove one of the two"
    )]
    DriveTaken(String),
    #[error("cannot reach OneDrive ({0}); trying again")]
    Offline(String),
    /// A failure of the tree store, in its own words ("the tree store: …"), that ends
    /// the cycle and stops the folder (quality finding `RE6`). Not every store failure ends a cycle: those the
    /// read-write reconcile only logs and passes over after its commit do not come here.
    #[error("{0}")]
    Store(String),
    #[error("the folder could not be brought up to date: {0}")]
    Apply(String),
    #[error("cancelled")]
    Cancelled,
}

impl CycleError {
    /// Trouble that stops the folder until someone acts.
    pub(crate) fn blocking(&self) -> bool {
        matches!(
            self,
            CycleError::SignedOut | CycleError::OtherAccount(_) | CycleError::DriveTaken(_) | CycleError::Store(_)
        )
    }
}

impl CycleError {
    /// The kind of trouble this is for the account's overall state: only
    /// [`Offline`](CycleError::Offline) is OneDrive out of reach, whatever its sentence says.
    pub(crate) fn kind(&self) -> TroubleKind {
        match self {
            CycleError::Offline(_) => TroubleKind::Unreachable,
            _ => TroubleKind::Other,
        }
    }
}

impl From<TreeError> for CycleError {
    fn from(e: TreeError) -> Self {
        CycleError::Store(e.to_string())
    }
}
