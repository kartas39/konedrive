//! The run in which OneDrive is a state of its own ([`RunKind::RequestsInFlight`]). A
//! request of a step changes it when it is sent, whatever becomes of its answer: the answer
//! comes at once, late — after the user and another client went on —, or never, and then
//! only a listing says what OneDrive has. A listing reads it: its page is fetched at one
//! time and written at another, before or after the answers that came in between.
//!
//! What is asked of such a run: the rules of the rows after every step and every page; no
//! step that must not leave ([`step_rules`](super::step_rules)); no commit of a
//! directory's removal written against the rows; and, once every request is settled, that
//! the plan ends, nothing is lost of what the rows had then, and the rows say OneDrive has
//! what it has.

use super::drive::RequestOutcome;
use super::step_rules::find_what_a_directory_removal_must_not_take;
use super::*;
use crate::{ListedChangeOutcome, ListingId};

/// For how many rounds of one run requests are left in flight.
const ROUNDS_IN_FLIGHT: usize = 3;

/// A request that left and whose step has not ended.
#[derive(Debug, Clone)]
struct RequestInFlight {
    step: OneDriveStep,
    /// OneDrive's answer, when the request reached it already: the answer is slow. `None`
    /// while the request itself is on its way.
    answer: Option<RequestOutcome>,
    rounds_left: usize,
}

/// The requests that are not settled, and the page a listing fetched and did not write yet.
#[derive(Debug, Default)]
pub(super) struct RequestsInFlight {
    in_flight: Vec<RequestInFlight>,
    /// The requests OneDrive took whose answer was lost: on record until a listing ended
    /// that began after them.
    lost: Vec<OneDriveStep>,
    /// The listing, its page, and the count of confirmations when the page was fetched.
    held_page: Option<(ListingId, Vec<Change>, u64)>,
    /// The items of OneDrive a refused request named: the caller reads each again, and
    /// gives what it read with the next listing.
    read_again: Vec<String>,
}

impl World {
    fn drive(&mut self) -> &mut OneDriveChanges {
        self.drive.as_mut().expect("OneDrive is a state of its own in this run")
    }

    fn check_rules_of_rows(&mut self, after: &str) -> Result<(), Failure> {
        let is_listing_under_way = self.store.item_store().find_listing_under_way().unwrap().is_some();
        find_broken_rule(&rows(&mut self.store), is_listing_under_way).map_or(Ok(()), |rule| Err(format!("after {after}: {rule}")))
    }

    /// What the caller knows of the requests that are not settled: the items they are
    /// about, and the directories they send into.
    fn tell_requests_not_settled(&mut self) {
        let steps: Vec<SyncStep> = self.requests.in_flight.iter().map(|request| &request.step).chain(&self.requests.lost).cloned().map(SyncStep::OneDrive).collect();
        self.facts.items_with_running_step = steps.iter().map(SyncStep::local_item_id).collect();
        self.facts.destinations_of_running_steps = steps.iter().filter_map(|step| Self::destination_of(&mut self.store, step)).collect();
    }

    /// The end of a step whose request OneDrive answered with `answer`: its commit, where
    /// OneDrive did what was asked. The commit of a directory's removal is never written
    /// against the rows.
    fn end_step(&mut self, step: &OneDriveStep, answer: RequestOutcome) -> Result<StepOutcome, Failure> {
        let outcome = match answer {
            RequestOutcome::NameTaken => {
                self.stand_in.remember_name_taken(step);
                StepOutcome::NameTaken
            }
            RequestOutcome::Refused => {
                // A refusal decides nothing; the items it was about are read again.
                let parent = match step {
                    OneDriveStep::MakeDirectory { parent, .. } | OneDriveStep::SendNewFile { parent, .. } | OneDriveStep::SendMove { parent, .. } => Some(parent),
                    OneDriveStep::SendContent { .. } | OneDriveStep::SendFileRemoval { .. } | OneDriveStep::SendDirectoryRemoval { .. } => None,
                };
                let named = step.item().onedrive_item.iter().chain(parent).map(|item| item.item_id.as_str().to_owned()).filter(|id| id != drive::ROOT_ID);
                self.requests.read_again.extend(named);
                StepOutcome::NotDone
            }
            RequestOutcome::Confirmed(confirmed) => {
                let against_the_rows = match step {
                    OneDriveStep::SendDirectoryRemoval { item, .. } => find_what_a_directory_removal_must_not_take(&mut self.store, item.local_item_id, &BTreeSet::new()),
                    _ => None,
                };
                let outcome = self.stand_in.commit_confirmed(&mut self.store, step, confirmed.as_ref());
                if let (StepOutcome::Written, Some(taken)) = (outcome, against_the_rows) {
                    return Err(format!("the commit of the removal of the directory {} was written though the rows said OneDrive has below it {taken}", step.item().local_item_id));
                }
                outcome
            }
        };
        self.check_rules_of_rows(&format!("{step:?}"))?;
        Ok(outcome)
    }

    /// What the step asks before its request leaves. A step of a plan made now that its
    /// own guard refuses is a plan and a guard that do not say the same.
    fn check_request_can_leave(&mut self, step: &OneDriveStep) -> Result<(), Failure> {
        match step {
            OneDriveStep::SendDirectoryRemoval { item, below } if !self.store.item_store().can_directory_removal_be_sent(item, below, &self.facts.items_with_running_step, &self.facts.destinations_of_running_steps).unwrap() => {
                Err(format!("the removal of the directory {} is a step of the plan, and the store answers that it cannot be sent", item.local_item_id))
            }
            _ => Ok(()),
        }
    }

    /// Runs `step` to its end now. Where OneDrive is a state of its own, a step towards it
    /// asks, sends its request to it, and commits what it answered; elsewhere OneDrive is
    /// the rows.
    pub(super) fn run_step(&mut self, step: &SyncStep) -> Result<StepOutcome, Failure> {
        match (step, self.drive.is_some()) {
            (SyncStep::OneDrive(sending), true) => {
                self.check_request_can_leave(sending)?;
                let answer = self.drive().request(sending);
                self.end_step(sending, answer)
            }
            (SyncStep::OneDrive(OneDriveStep::SendDirectoryRemoval { item, .. }), false) => {
                let against_the_rows = find_what_a_directory_removal_must_not_take(&mut self.store, item.local_item_id, &BTreeSet::new());
                match (self.stand_in.run(&mut self.store, step), against_the_rows) {
                    (StepOutcome::Written, Some(taken)) => Err(format!("the commit of the removal of the directory {} was written though the rows said OneDrive has below it {taken}", item.local_item_id)),
                    (outcome, _) => Ok(outcome),
                }
            }
            _ => Ok(self.stand_in.run(&mut self.store, step)),
        }
    }

    /// The request of `step` leaves, and its step does not end now: the request is slow,
    /// its answer is, or its answer is lost.
    fn let_request_leave(&mut self, step: &OneDriveStep) -> Result<(), Failure> {
        self.check_request_can_leave(step)?;
        let rounds_left = self.what_facts_are_drawn_with.below(2);
        match self.what_facts_are_drawn_with.below(3) {
            0 => self.requests.in_flight.push(RequestInFlight { step: step.clone(), answer: None, rounds_left }),
            1 => {
                let answer = Some(self.drive().request(step));
                self.requests.in_flight.push(RequestInFlight { step: step.clone(), answer, rounds_left });
            }
            _ => {
                let answer = self.drive().request(step);
                self.drive().forget_what_was_sent(&answer);
                self.requests.lost.push(step.clone());
            }
        }
        self.tell_requests_not_settled();
        Ok(())
    }

    /// The requests in flight whose time has come reach OneDrive, where they did not yet,
    /// and their steps end; all of them when `all` says so.
    fn let_requests_arrive(&mut self, all: bool) -> Result<(), Failure> {
        let (arrived, in_flight): (Vec<_>, Vec<_>) = std::mem::take(&mut self.requests.in_flight).into_iter().partition(|request| all || request.rounds_left == 0);
        self.requests.in_flight = in_flight.into_iter().map(|request| RequestInFlight { rounds_left: request.rounds_left - 1, ..request }).collect();
        for request in arrived {
            let answer = match request.answer {
                Some(answer) => answer,
                None => self.drive().request(&request.step),
            };
            self.end_step(&request.step, answer)?;
        }
        self.tell_requests_not_settled();
        Ok(())
    }

    /// Writes `page` of `listing`, fetched at the count `fetched_at`, and reads again from
    /// OneDrive what it leaves, until nothing is left: the listing has then ended.
    fn write_page(&mut self, listing: ListingId, mut page: Vec<Change>, mut fetched_at: u64) -> Result<(), Failure> {
        for _ in 0..10 {
            let outcome = self.store.item_store().write_listed_changes(listing, &page, fetched_at, &ListingLink::DeltaLink("link".into())).map_err(|error| format!("a page is refused: {error}"))?;
            self.check_rules_of_rows(&format!("the page {page:?}, which answered {outcome:?}"))?;
            if let Some(refused) = outcome.changes.iter().find(|change| matches!(change, ListedChangeOutcome::Refused(_))) {
                return Err(format!("a change of the page {page:?} is refused: {refused:?}"));
            }
            if outcome.is_link_written() {
                return Ok(());
            }
            fetched_at = self.store.item_store().confirmation_count().unwrap();
            page = outcome.outdated_items.iter().map(|item| self.drive().read(item.as_str())).collect();
        }
        Err("a page is left to be read again without end".to_owned())
    }

    fn write_held_page(&mut self) -> Result<(), Failure> {
        match self.requests.held_page.take() {
            Some((listing, page, fetched_at)) => self.write_page(listing, page, fetched_at),
            None => Ok(()),
        }
    }

    /// A listing of what OneDrive changed since the one before, fetched and written now,
    /// with what was read again of the items a refused request named. It began after
    /// every request whose answer was lost: those are settled. Whether it said anything.
    pub(super) fn list_drive_changes(&mut self) -> Result<bool, Failure> {
        let listing = self.store.item_store().begin_delta_listing().unwrap();
        let mut page = self.drive().fetch_changes();
        for item in std::mem::take(&mut self.requests.read_again) {
            let read = self.drive().read(&item);
            page.push(read);
        }
        let said_anything = !page.is_empty();
        self.write_page(listing.id, page, listing.began_at_count)?;
        self.requests.lost.clear();
        self.tell_requests_not_settled();
        Ok(said_anything)
    }

    /// Both sides go on at what a request in flight is about, each change one that a rule
    /// of the plan or of a commit is there for.
    fn change_what_a_request_is_about(&mut self) {
        let steps: Vec<OneDriveStep> = self.requests.in_flight.iter().map(|request| &request.step).chain(&self.requests.lost).cloned().collect();
        let Some(step) = self.what_facts_are_drawn_with.pick(&steps).cloned() else { return };
        let item = step.item().local_item_id;
        let binding = |versions: &ItemVersions| versions.onedrive_item.as_ref().map(|item| item.item_id.as_str().to_owned());
        let number = self.round_count * 100 + self.requests.in_flight.len();
        match &step {
            // A program saves the file again, by a rename over it, while its content is sent.
            OneDriveStep::SendContent { .. } | OneDriveStep::SendNewFile { .. } => self.changes.save_over(&mut self.store, item),
            // The user takes the item back out of the directory it is sent into and removes
            // that directory; or another client removes it.
            OneDriveStep::SendMove { parent, .. } | OneDriveStep::MakeDirectory { parent, .. } => {
                let Some(directory) = self.store.item_store().find_bound_item(parent).unwrap().filter(|directory| directory.local_item_id != ROOT) else { return };
                if self.what_facts_are_drawn_with.below(2) == 0 {
                    self.changes.move_to_root_and_remove(&mut self.store, item, directory.local_item_id);
                } else {
                    let id = parent.item_id.as_str().to_owned();
                    self.changes.said.push(format!("OneDrive: removes {id}, which a request sends an item into"));
                    self.drive().remove(&id);
                }
            }
            // Another client makes a file in the directory whose removal is on its way, or
            // moves an item into it.
            OneDriveStep::SendDirectoryRemoval { item, .. } => {
                let Some(directory) = binding(item).filter(|id| self.drive().items.contains_key(id)) else { return };
                let moved = self.drive().items.iter().find(|(id, moved)| moved.parent == drive::ROOT_ID && **id != directory && moved.kind == Kind::File).map(|(id, moved)| (id.clone(), moved.clone()));
                match moved.filter(|_| self.what_facts_are_drawn_with.below(2) == 0) {
                    Some((id, moved)) => {
                        if self.drive().put(&id, drive::DriveItem { parent: directory.clone(), ..moved }) {
                            self.changes.said.push(format!("OneDrive: moves {id} into {directory}, whose removal is on its way"));
                        }
                    }
                    None => {
                        let made = drive::DriveItem::new(&directory, &format!("made{number}"), Kind::File, Some((700 + number as u64, 9700)));
                        if self.drive().put(&format!("Y{number}"), made) {
                            self.changes.said.push(format!("OneDrive: makes Y{number} in {directory}, whose removal is on its way"));
                        }
                    }
                }
            }
            // The user removes the directory the item is in, with all in it.
            OneDriveStep::SendFileRemoval { item } => {
                let Some(directory) = item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()).map(|place| place.parent_local_item_id).filter(|directory| *directory != ROOT) else { return };
                self.changes.remove_with_all_in_it(&mut self.store, directory);
            }
        }
    }

    /// One round: the page held and the answers that are due come, in either order; a plan
    /// is made, and each of its steps runs now or lets its request leave; and while any is
    /// in flight both sides go on, and a listing fetches its page.
    fn run_round_with_requests_in_flight(&mut self) -> Result<(), Failure> {
        self.round_count += 1;
        let is_page_first = self.what_facts_are_drawn_with.below(2) == 0;
        if is_page_first {
            self.write_held_page()?;
        }
        self.let_requests_arrive(false)?;
        self.write_held_page()?;
        self.facts.places_taken_in_onedrive = self.stand_in.places_taken_in_onedrive.clone();
        let (_, plan) = plan_next_steps(&mut self.store, &self.facts, &mut self.widening);
        if !plan.faults.is_empty() {
            return Err(format!("the plan says it is at fault while requests are in flight: {:?}\n  waits {:?}", plan.faults, plan.waits));
        }
        let mut steps = plan.steps.clone();
        self.step_order.shuffle(&mut steps);
        for step in &steps {
            if let Some(refused) = step_rules::find_step_that_must_not_leave(&mut self.store, &self.facts, self.drive.as_ref(), step) {
                return Err(refused);
            }
            match step {
                SyncStep::OneDrive(sending) if self.what_facts_are_drawn_with.below(2) == 0 => self.let_request_leave(sending)?,
                // A step that finds the rows changed, or that OneDrive refuses, decides
                // nothing: the plan made next does.
                _ => drop(self.run_step(step)?),
            }
            self.check_rules_of_rows(&format!("{step:?}"))?;
        }
        if !self.requests.in_flight.is_empty() || !self.requests.lost.is_empty() {
            self.change_what_a_request_is_about();
            for _ in 0..2 {
                self.changes.change_folder(&mut self.store);
                let drive = self.drive.as_mut().expect("OneDrive is a state of its own in this run");
                self.changes.change_onedrive(drive);
            }
            if self.requests.held_page.is_none() {
                let listing = self.store.item_store().begin_delta_listing().unwrap();
                let page = self.drive().fetch_changes();
                // The page may be written before the answers that are on their way: what
                // those requests sent is then known by the listing first.
                for answer in self.requests.in_flight.iter().filter_map(|request| request.answer.clone()).collect::<Vec<_>>() {
                    self.drive().forget_what_was_sent(&answer);
                }
                self.requests.held_page = Some((listing.id, page, listing.began_at_count));
            }
        }
        Ok(())
    }

    /// The rows say OneDrive has what it has: every item of it, where it is, with its
    /// content; and nothing else.
    fn check_rows_say_what_onedrive_has(&mut self) -> Result<(), Failure> {
        let rows = rows(&mut self.store);
        let item_id_of = |id: LocalItemId| rows.iter().find(|row| row.local_item_id == id).and_then(|row| row.onedrive_item.as_ref()).map(|item| item.item_id.as_str().to_owned());
        let said: BTreeMap<String, (String, String, Option<String>)> = rows
            .iter()
            .filter(|row| row.local_item_id != ROOT)
            .filter_map(|row| {
                let place = row.onedrive.as_ref()?.place.as_ref()?;
                Some((item_id_of(row.local_item_id)?, (item_id_of(place.parent_local_item_id)?, place.name.to_string_lossy().into_owned(), content_of(&row.onedrive))))
            })
            .collect();
        let has: BTreeMap<String, (String, String, Option<String>)> = self.drive().items.iter().map(|(id, item)| (id.clone(), (item.parent.clone(), item.name.clone(), item.content.map(|(size, mtime)| format!("h{size}-{mtime}"))))).collect();
        if said != has {
            let differs: Vec<String> = said.keys().chain(has.keys()).filter(|id| said.get(*id) != has.get(*id)).map(|id| format!("{id}: the rows say {:?}, OneDrive has {:?}", said.get(id), has.get(id))).collect();
            return Err(format!("the rows do not say what OneDrive has: {}", differs.join("; ")));
        }
        Ok(())
    }

    /// As [`change_and_converge`](Self::change_and_converge), with OneDrive a state of its
    /// own and requests in flight for a few rounds; then every request is settled, and the
    /// plan and the listing run until neither has anything left.
    pub(super) fn change_and_converge_with_requests_in_flight(&mut self, here: usize, in_onedrive: usize) -> Result<(), Failure> {
        for _ in 0..here {
            self.changes.change_folder(&mut self.store);
        }
        let drive = self.drive.as_mut().expect("OneDrive is a state of its own in this run");
        self.changes.make_case_on_purpose(&mut self.store, drive);
        for _ in 0..in_onedrive {
            self.changes.change_onedrive(drive);
        }
        self.list_drive_changes()?;
        for _ in 0..ROUNDS_IN_FLIGHT {
            self.run_round_with_requests_in_flight()?;
        }
        // Every request is settled: the answers come, the page held is written, and a
        // listing that began after the lost answers says what OneDrive has.
        self.let_requests_arrive(true)?;
        self.write_held_page()?;
        self.list_drive_changes()?;
        let expected = ExpectedEnd::of(&mut self.store);
        for _ in 0..10 {
            self.run_to_the_end()?;
            if !self.list_drive_changes()? {
                self.check_converged(&expected)?;
                return self.check_rows_say_what_onedrive_has();
            }
        }
        Err("the plan and the listing do not end".to_owned())
    }
}
