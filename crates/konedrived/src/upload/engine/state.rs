//! The worker's own state, in parts: each part is one type that keeps its fields to
//! itself, and the worker's files ask it through its methods.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use konedrive_graph::drive::MAX_RETRY_AFTER;
use konedrive_tree::outbox::{Picked, Reason};

use super::marks::Marks;
use super::outcome::Class;
use crate::upload::{OutboxCounts, Upload, BACKOFF_MAX, THROTTLE_FIRST};

/// OneDrive asked the whole account to wait (§4.10).
pub(super) struct Throttle {
    /// Nothing is sent before this (Unix seconds).
    until: i64,
    /// OneDrive gave no time for the wait under way: the worker chose it.
    guessed: bool,
    /// The wait of the next refusal that names none.
    step: Duration,
}

impl Throttle {
    pub fn new() -> Self {
        Self { until: 0, guessed: false, step: THROTTLE_FIRST }
    }

    /// OneDrive refused a request at `now` and asked to wait `asked` (`Retry-After`, at
    /// most an hour), or named no time. The rules, while a wait is under way:
    ///
    /// - a time OneDrive named beats one the worker chose: it takes the place of a chosen
    ///   wait even when it is shorter — OneDrive said when;
    /// - of two times OneDrive named, the later end stands: a wait is never cut short by an
    ///   answer to another row that was in flight;
    /// - a refusal that names no time adds nothing: the rows refused together are one
    ///   throttle.
    ///
    /// With no wait under way a refusal that names no time waits 10 s, doubling with each
    /// such throttle up to an hour, until a row goes through ([`passed`](Self::passed)).
    pub fn refused(&mut self, asked: Option<Duration>, now: i64) {
        let under_way = self.holds(now);
        match asked {
            Some(wait) => {
                let until = now + wait.min(MAX_RETRY_AFTER).as_secs().max(1) as i64;
                if !under_way || self.guessed || until > self.until {
                    self.until = until;
                }
                self.guessed = false;
            }
            None if under_way => {}
            None => {
                self.until = now + self.step.as_secs().max(1) as i64;
                self.guessed = true;
                self.step = (self.step * 2).min(BACKOFF_MAX);
            }
        }
    }

    /// A row went through: the next throttle that names no time starts at 10 s again.
    pub fn passed(&mut self) {
        self.step = THROTTLE_FIRST;
    }

    /// When the wait ends, while it lasts at `now`.
    pub fn until(&self, now: i64) -> Option<i64> {
        (self.until > now).then_some(self.until)
    }

    pub fn holds(&self, now: i64) -> bool {
        self.until > now
    }
}

/// What keeps the worker from sending besides the throttle and the pause, each in a slot
/// of its own, set and cleared by the one place that knows it.
#[derive(Default)]
pub(super) struct Trouble {
    /// The token source said the account is signed out: nothing more is taken. Never
    /// cleared: the sign-out stops the folder's sync, and this worker with it.
    signed_out: bool,
    /// A fault point fired: nothing more is taken until the worker is built again.
    crashed: bool,
    /// Why the folder could not be opened at the last drain: nothing is taken until a
    /// drain opens it.
    folder: Option<String>,
    /// Why the write gate was closed when it was last asked.
    gate: Option<String>,
}

impl Trouble {
    pub fn sign_out(&mut self) {
        self.signed_out = true;
    }

    pub fn crash(&mut self) {
        self.crashed = true;
    }

    /// The folder was opened at a drain (`None`), or why it was not.
    pub fn folder(&mut self, error: Option<String>) {
        self.folder = error;
    }

    pub fn folder_closed(&self) -> Option<String> {
        self.folder.clone()
    }

    /// Whether the worker takes nothing: for as long as it lives (signed out, a fault
    /// point), or until a drain opens the folder.
    pub fn holds(&self) -> bool {
        self.signed_out || self.crashed || self.folder.is_some()
    }

    /// The write gate's answer: open (`None`), or why it is closed. Whether this is a new
    /// reason for it to be closed, which the journal gets once.
    pub fn gate(&mut self, closed: Option<String>) -> bool {
        let new = closed.is_some() && self.gate != closed;
        self.gate = closed;
        new
    }
}

/// The delta cycle the outbox waits for (`docs/design/writes.md` §3 and §4.9: the cycle
/// before the outbox).
pub(super) struct Cycle {
    /// A cycle has gone through since the worker was told to wait for one.
    done: bool,
    /// The network came back: rows in backoff go once the cycle is done.
    network_back: bool,
}

impl Cycle {
    pub fn new() -> Self {
        Self { done: true, network_back: false }
    }

    pub fn wait(&mut self, network_back: bool) {
        self.done = false;
        self.network_back |= network_back;
    }

    /// A cycle went through. Whether the rows in backoff go now: after the cycle the
    /// worker waited for, and after the network came back.
    pub fn went_through(&mut self) -> bool {
        let waited = !self.done;
        self.done = true;
        waited || std::mem::take(&mut self.network_back)
    }

    pub fn done(&self) -> bool {
        self.done
    }
}

/// One row the worker holds: taken, and not settled yet.
pub(super) struct Flight {
    pub class: Class,
    pub rel: PathBuf,
    /// The row's reason when it was taken: an `upload-failed` event is
    /// written once per row and reason.
    pub reason: Option<Reason>,
}

/// The rows in flight, and how far their content is.
#[derive(Default)]
pub(super) struct Flights {
    rows: HashMap<i64, (Flight, Option<(u64, u64)>)>,
}

impl Flights {
    pub fn took(&mut self, seq: i64, flight: Flight) {
        self.rows.insert(seq, (flight, None));
    }

    pub fn landed(&mut self, seq: i64) -> Option<Flight> {
        self.rows.remove(&seq).map(|(flight, _)| flight)
    }

    /// The worker stopped: it holds no row any more.
    pub fn clear(&mut self) {
        self.rows.clear();
    }

    pub fn progress(&mut self, seq: i64, sent: u64, total: u64) {
        if let Some((_, upload)) = self.rows.get_mut(&seq) {
            *upload = Some((sent, total));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn seqs(&self) -> HashSet<i64> {
        self.rows.keys().copied().collect()
    }

    /// The rows of `class` in flight.
    pub fn of(&self, class: Class) -> usize {
        self.rows.values().filter(|(flight, _)| flight.class == class).count()
    }

    /// The content going up, in the rows' order.
    pub fn uploads(&self) -> Vec<Upload> {
        let mut uploads: Vec<(i64, Upload)> = self
            .rows
            .iter()
            .filter_map(|(&seq, (flight, upload))| upload.map(|(sent, total)| (seq, Upload { rel: flight.rel.clone(), sent, total })))
            .collect();
        uploads.sort_by_key(|(seq, _)| *seq);
        uploads.into_iter().map(|(_, upload)| upload).collect()
    }
}

/// Everything the worker's loop keeps between its looks, under one lock.
pub(super) struct Shared {
    pub throttle: Throttle,
    pub trouble: Trouble,
    pub cycle: Cycle,
    pub flights: Flights,
    pub marks: Marks,
    /// The rows a `403` blocked were let go once, when this worker could first send
    /// ([`Engine::release_forbidden`](super::Engine::release_forbidden)).
    pub forbidden_released: bool,
    /// What the last pick found in front of the rows that could not run.
    pub waits: Picked,
    /// What the outbox held when the worker last looked.
    pub counts: OutboxCounts,
    /// No session given up is cancelled before this (Unix seconds): a
    /// cancel failed (issue #47).
    pub cancel_after: i64,
}

impl Shared {
    pub fn new() -> Self {
        Self {
            throttle: Throttle::new(),
            trouble: Trouble::default(),
            cycle: Cycle::new(),
            flights: Flights::default(),
            marks: Marks::default(),
            forbidden_released: false,
            waits: Picked::default(),
            counts: OutboxCounts::default(),
            cancel_after: 0,
        }
    }
}

#[cfg(test)]
mod tests;
