//! What the run checks against OneDrive itself: the service's behaviour that the write design
//! assumes (§15) and the wiremock suites can only take as given. Every write goes through
//! konedrive's own `DriveClient`, so the requests are the ones the outbox worker sends.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use konedrived::drive::{
    ChunkOutcome, DeltaFrom, DeltaNext, DriveClient, DriveError, DriveItem, ItemChange, UploadTarget, WriteError, CHUNK_SIZE,
};
use konedrived::quickxor::QuickXor;
use serde_json::{json, Value};

use crate::guard::{Guard, TOP};
use crate::harness::{Api, Options};

/// The time every file is given (`fileSystemInfo`), plus an offset per write: 2023-11-14.
const T0: i64 = 1_700_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pass(String),
    Fail(String),
    /// Done, but the result has to be looked at by hand.
    Manual(String),
}

impl Outcome {
    pub fn word(&self) -> &'static str {
        match self {
            Outcome::Pass(_) => "PASS",
            Outcome::Fail(_) => "FAIL",
            Outcome::Manual(_) => "LOOK",
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Outcome::Pass(detail) | Outcome::Fail(detail) | Outcome::Manual(detail) => detail,
        }
    }
}

use Outcome::{Fail, Manual, Pass};

/// Prints one outcome as the run goes.
pub fn show(name: &str, outcome: &Outcome) {
    println!("  {}  {name}: {}", outcome.word(), outcome.detail());
}

/// Makes `/konedrive-write-test` if it is not there, then the run's own folder in it: the only
/// two writes the guard lets through before the run folder exists.
pub async fn bootstrap(drive: &DriveClient, guard: &Guard, root: &str, run_id: &str) -> Result<String, String> {
    match drive.create_folder(root, TOP).await {
        Ok(_) | Err(WriteError::NameExists) => {}
        Err(e) => return Err(format!("making /{TOP}: {e}")),
    }
    let top = drive.child(root, TOP).await.map_err(|e| format!("reading /{TOP}: {e}"))?;
    if top.folder.is_none() {
        return Err(format!("/{TOP} is not a folder"));
    }
    let folder = drive.create_folder(&top.id, run_id).await.map_err(|e| format!("making /{TOP}/{run_id}: {e}"))?;
    if guard.run_folder().as_deref() != Some(folder.id.as_str()) {
        return Err("the guard did not learn the run folder from OneDrive's answer".into());
    }
    Ok(folder.id)
}

pub fn quickxor(bytes: &[u8]) -> String {
    let mut hash = QuickXor::new();
    hash.update(bytes);
    hash.finish_base64()
}

/// Pulls a `Result` out, or ends the check with a failure that says what was being done.
macro_rules! step {
    ($what:expr, $result:expr) => {
        match $result {
            Ok(value) => value,
            Err(e) => return Fail(format!("{}: {e}", $what)),
        }
    };
}

pub struct Run {
    drive: DriveClient,
    read_only: DriveClient,
    api: Api,
    guard: Arc<Guard>,
    folder: String,
    run_id: String,
    delta_link: String,
    delta_wait: Duration,
    /// Every file the run wrote, by id: its name and the eTag its last write answered.
    files: HashMap<String, (String, Option<String>)>,
    gone: HashSet<String>,
    seed: u64,
}

impl Run {
    pub fn new(
        drive: DriveClient,
        read_only: DriveClient,
        api: Api,
        guard: Arc<Guard>,
        folder: String,
        options: &Options,
        delta_link: String,
    ) -> Run {
        let seed = options.run_id.bytes().fold(0x9e37_79b9_7f4a_7c15_u64, |seed, b| seed.rotate_left(5) ^ u64::from(b));
        Run {
            drive,
            read_only,
            api,
            guard,
            folder,
            run_id: options.run_id.clone(),
            delta_link,
            delta_wait: options.delta_wait,
            files: HashMap::new(),
            gone: HashSet::new(),
            seed: seed | 1,
        }
    }

    /// Every check, in order, each printed as it ends. They stop at the guard's first refusal.
    pub async fn all(&mut self) -> Vec<(&'static str, Outcome)> {
        let mut done = Vec::new();
        macro_rules! check {
            ($name:expr, $call:expr) => {
                if self.guard.refused().is_none() {
                    let outcome = $call.await;
                    show($name, &outcome);
                    done.push(($name, outcome));
                }
            };
        }
        check!("an empty file: conflictBehavior=fail in the URL refuses a taken name", self.empty_file_name_taken());
        check!("an empty file: If-Match is honoured on a 0-byte PUT", self.empty_file_if_match());
        check!("a small file: a one-request session, its guards, its time and its hash", self.small_file());
        check!("a folder's cTag guards its delete", self.folder_delete_guard());
        check!("a rename to a name that is taken is refused 409", self.rename_to_taken_name());
        check!("names collide whatever their case", self.case_collisions());
        check!("a large file: 10 MiB fragments, and a resume from the session's status", self.large_file());
        check!("an edit in OneDrive during a session, before its last fragment", self.edit_during_a_session());
        check!("a delete goes to the recycle bin", self.recycle_bin());
        check!("the token exported after the switch back to read-only cannot write", self.read_only_token());
        check!("delta returns the run's own changes with the eTags they were given", self.delta_echo());
        done
    }

    fn note(&mut self, item: &DriveItem) {
        if item.file.is_some() {
            self.files.insert(item.id.clone(), (item.name.clone().unwrap_or_default(), item.e_tag.clone()));
            self.gone.remove(&item.id);
        }
    }

    /// Pseudo-random content, different for every run.
    fn content(&mut self, len: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(len + 8);
        while bytes.len() < len {
            self.seed ^= self.seed << 13;
            self.seed ^= self.seed >> 7;
            self.seed ^= self.seed << 17;
            bytes.extend_from_slice(&self.seed.to_le_bytes());
        }
        bytes.truncate(len);
        bytes
    }

    async fn new_file(&mut self, parent: &str, name: &str, content: Vec<u8>, time: i64) -> Result<DriveItem, WriteError> {
        let item = self.drive.upload_small(UploadTarget::New { parent_id: parent, name }, content, time).await?;
        self.note(&item);
        Ok(item)
    }

    async fn put(&mut self, name: &str, content: Vec<u8>, time: i64) -> Result<DriveItem, WriteError> {
        let folder = self.folder.clone();
        self.new_file(&folder, name, content, time).await
    }

    async fn replace(&mut self, id: &str, if_match: &str, content: Vec<u8>, time: i64) -> Result<DriveItem, WriteError> {
        let item = self.drive.upload_small(UploadTarget::Existing { id, if_match }, content, time).await?;
        self.note(&item);
        Ok(item)
    }

    async fn rename(&mut self, item: &DriveItem, name: &str) -> Result<DriveItem, WriteError> {
        let etag = item.e_tag.clone().unwrap_or_default();
        let change = ItemChange { name: Some(name), ..ItemChange::default() };
        let renamed = self.drive.update_item(&item.id, &etag, &change).await?;
        self.note(&renamed);
        Ok(renamed)
    }

    /// §3.6: a new empty file is a `PUT` with `conflictBehavior=fail` in its URL, because a
    /// `PUT`'s default is to replace.
    async fn empty_file_name_taken(&mut self) -> Outcome {
        let first = step!("the first PUT", self.put("empty.txt", Vec::new(), T0).await);
        if first.size != Some(0) {
            return Fail(format!("the new file has {:?} bytes", first.size));
        }
        match self.put("empty.txt", Vec::new(), T0 + 60).await {
            Err(WriteError::NameExists) => Pass("a second PUT of the same name was refused 409".into()),
            Ok(second) => Fail(format!(
                "a second PUT of the same name went through (item {}, the first was {}): the URL's conflictBehavior=fail is not honoured",
                second.id, first.id
            )),
            Err(e) => Fail(format!("the second PUT: {e}")),
        }
    }

    /// §15, assumed: `If-Match` is honoured on the `PUT` that empties a file.
    async fn empty_file_if_match(&mut self) -> Outcome {
        let item = step!("the new file", self.put("if-match.txt", Vec::new(), T0).await);
        let old = item.e_tag.clone().unwrap_or_default();
        let edited = step!("the change standing for an edit in OneDrive", self.replace(&item.id, &old, b"edited".to_vec(), T0 + 60).await);
        let current = edited.e_tag.clone().unwrap_or_default();
        match self.replace(&item.id, &old, Vec::new(), T0 + 120).await {
            Err(WriteError::Changed) => {}
            Ok(emptied) => {
                return Fail(format!(
                    "a 0-byte PUT with the eTag from before the edit went through ({:?} bytes now): If-Match is not honoured there",
                    emptied.size
                ))
            }
            Err(e) => return Fail(format!("the 0-byte PUT with the old eTag: {e}")),
        }
        match self.replace(&item.id, &current, Vec::new(), T0 + 120).await {
            Ok(emptied) if emptied.size == Some(0) => {
                Pass("a 0-byte PUT with the old eTag was refused 412; with the current one it went through".into())
            }
            Ok(emptied) => Fail(format!("the 0-byte PUT went through, but the file has {:?} bytes", emptied.size)),
            Err(e) => Fail(format!("the 0-byte PUT with the current eTag: {e}")),
        }
    }

    /// §3.6: a file up to 10 MiB goes in a one-request session, carrying its time.
    async fn small_file(&mut self) -> Outcome {
        let content = self.content(100 * 1024);
        let hash = quickxor(&content);
        let item = step!("the upload", self.put("small.bin", content.clone(), T0 + 3600).await);
        if item.size != Some(content.len() as u64) || item.quick_xor_hash() != Some(hash.as_str()) {
            return Fail(format!("OneDrive has {:?} bytes with hash {:?}; {} were sent, hash {hash}", item.size, item.quick_xor_hash(), content.len()));
        }
        if item.mtime() != T0 + 3600 {
            return Fail(format!("the file's time is {}, not the {} sent in fileSystemInfo", item.mtime(), T0 + 3600));
        }
        match self.put("small.bin", b"another".to_vec(), T0).await {
            Err(WriteError::NameExists) => {}
            Ok(other) => return Fail(format!("a second new file of the same name went through, as {:?}", other.name)),
            Err(e) => return Fail(format!("the second new file of the same name: {e}")),
        }
        let old = item.e_tag.clone().unwrap_or_default();
        step!("the change", self.replace(&item.id, &old, b"changed".to_vec(), T0 + 3660).await);
        match self.replace(&item.id, &old, b"again".to_vec(), T0 + 3720).await {
            Err(WriteError::Changed) => Pass(
                "100 KiB went up in one request with its time and hash; a second new file of the name was refused 409, \
                 a session for a changed file with an old eTag 412"
                    .into(),
            ),
            Ok(_) => Fail("a session with an old eTag went through".into()),
            Err(e) => Fail(format!("the session with an old eTag: {e}")),
        }
    }

    /// §3.6, assumed: on a personal drive a folder's cTag changes with anything inside it, and
    /// guards the folder's delete.
    async fn folder_delete_guard(&mut self) -> Outcome {
        let folder = step!("the folder", self.drive.create_folder(&self.folder, "guarded").await);
        let inner = step!("the file inside", self.new_file(&folder.id, "inside.txt", b"one".to_vec(), T0).await);
        let before = step!("reading the folder", self.drive.item(&folder.id).await);
        let Some(old) = before.c_tag.clone() else { return Fail("OneDrive gave the folder no cTag".into()) };
        let inner_etag = inner.e_tag.clone().unwrap_or_default();
        step!("changing the file inside", self.replace(&inner.id, &inner_etag, b"two, longer".to_vec(), T0 + 60).await);
        let after = step!("reading the folder again", self.drive.item(&folder.id).await);
        let Some(current) = after.c_tag.clone() else { return Fail("OneDrive gave the folder no cTag the second time".into()) };
        if current == old {
            return Fail("the folder's cTag did not change when the file inside it did".into());
        }
        match self.drive.delete_item(&folder.id, &old).await {
            Err(WriteError::Changed) => {}
            Ok(()) => {
                self.gone.extend([folder.id.clone(), inner.id.clone()]);
                return Fail("the folder was deleted with its cTag from before the change inside it".into());
            }
            Err(e) => return Fail(format!("the delete with the old cTag: {e}")),
        }
        step!("the delete with the current cTag", self.drive.delete_item(&folder.id, &current).await);
        self.gone.extend([folder.id.clone(), inner.id.clone()]);
        let etag = if before.e_tag == after.e_tag { "kept" } else { "changed" };
        Pass(format!(
            "the cTag moved with the file inside; the old one was refused 412, the current one deleted the folder \
             (its eTag {etag} across the change)"
        ))
    }

    /// §15, assumed: a rename or move onto a name that is taken is refused 409.
    async fn rename_to_taken_name(&mut self) -> Outcome {
        let a = step!("a.txt", self.put("a.txt", b"a".to_vec(), T0).await);
        step!("b.txt", self.put("b.txt", b"b".to_vec(), T0).await);
        match self.rename(&a, "b.txt").await {
            Err(WriteError::NameExists) => Pass("renaming a.txt to b.txt, which is there, was refused 409".into()),
            Ok(renamed) => Fail(format!("the rename went through, as {:?}", renamed.name)),
            Err(e) => Fail(format!("the rename: {e}")),
        }
    }

    /// §15, assumed: OneDrive treats names without regard to case.
    async fn case_collisions(&mut self) -> Outcome {
        let one = step!("Case.txt", self.put("Case.txt", b"one".to_vec(), T0).await);
        match self.put("case.txt", b"two".to_vec(), T0).await {
            Err(WriteError::NameExists) => {}
            Ok(_) => return Fail("OneDrive keeps Case.txt and case.txt side by side".into()),
            Err(e) => return Fail(format!("making case.txt: {e}")),
        }
        let other = step!("other.txt", self.put("other.txt", b"three".to_vec(), T0).await);
        match self.rename(&other, "CASE.TXT").await {
            Err(WriteError::NameExists) => {}
            Ok(_) => return Fail("renaming other.txt to CASE.TXT went through beside Case.txt".into()),
            Err(e) => return Fail(format!("renaming other.txt to CASE.TXT: {e}")),
        }
        match self.rename(&one, "CASE.txt").await {
            Ok(renamed) if renamed.name.as_deref() == Some("CASE.txt") => Pass(
                "case.txt beside Case.txt was refused 409, and so was a rename to CASE.TXT; \
                 renaming Case.txt to CASE.txt went through"
                    .into(),
            ),
            Ok(renamed) => Fail(format!("the case-only rename left the name {:?}", renamed.name)),
            Err(e) => Fail(format!("the case-only rename Case.txt → CASE.txt: {e}")),
        }
    }

    /// §4.8: above 10 MiB a session in 10 MiB fragments; after an interruption the session's
    /// status says where to go on from.
    async fn large_file(&mut self) -> Outcome {
        let total = 3 * CHUNK_SIZE;
        let content = self.content(total as usize);
        let hash = quickxor(&content);
        let target = UploadTarget::New { parent_id: &self.folder, name: "large.bin" };
        let session = step!("the session", self.drive.create_upload_session(target, total, T0 + 7200).await);
        let first = CHUNK_SIZE as usize;
        match self.drive.upload_chunk(&session.url, 0, total, content[..first].to_vec()).await {
            Ok(ChunkOutcome::More(progress)) if progress.next == CHUNK_SIZE => {}
            Ok(ChunkOutcome::More(progress)) => return Fail(format!("after the first fragment OneDrive expects {}", progress.next)),
            Ok(ChunkOutcome::Done(_)) => return Fail("the session ended after its first fragment".into()),
            Err(e) => return Fail(format!("the first fragment: {e}")),
        }
        // What a worker that lost its place does: ask the session.
        let status = step!("the session's status", self.drive.upload_status(&session.url).await);
        if status.next != CHUNK_SIZE {
            return Fail(format!("the session's status says to go on from {}, not {CHUNK_SIZE}", status.next));
        }
        let mut next = status.next;
        let item = loop {
            let end = (next + CHUNK_SIZE).min(total);
            match self.drive.upload_chunk(&session.url, next, total, content[next as usize..end as usize].to_vec()).await {
                Ok(ChunkOutcome::More(progress)) if progress.next == end => next = end,
                Ok(ChunkOutcome::More(progress)) => return Fail(format!("after bytes up to {end} OneDrive expects {}", progress.next)),
                Ok(ChunkOutcome::Done(item)) if end == total => break *item,
                Ok(ChunkOutcome::Done(_)) => return Fail(format!("the session ended at {end} of {total} bytes")),
                Err(e) => return Fail(format!("the fragment from {next}: {e}")),
            }
        };
        self.note(&item);
        if item.size != Some(total) || item.quick_xor_hash() != Some(hash.as_str()) {
            return Fail(format!("OneDrive has {:?} bytes with hash {:?}; {total} were sent, hash {hash}", item.size, item.quick_xor_hash()));
        }
        Pass(format!(
            "{} MiB went up in three fragments of 10 MiB; after the first, the status said to go on from 10 MiB; the hash matches",
            total >> 20
        ))
    }

    /// §4.8 and limitations log F80, observed: `If-Match` is checked when a session is created,
    /// so an edit made in OneDrive before the last fragment is assumed to be superseded (kept in
    /// the version history). Either answer is safe; the detail records which one OneDrive gives.
    async fn edit_during_a_session(&mut self) -> Outcome {
        let item = step!("the file", self.put("window.bin", b"before".to_vec(), T0).await);
        let etag = item.e_tag.clone().unwrap_or_default();
        let total = 2 * CHUNK_SIZE;
        let content = self.content(total as usize);
        let hash = quickxor(&content);
        let target = UploadTarget::Existing { id: &item.id, if_match: &etag };
        let session = step!("the session", self.drive.create_upload_session(target, total, T0 + 60).await);
        let half = CHUNK_SIZE as usize;
        match self.drive.upload_chunk(&session.url, 0, total, content[..half].to_vec()).await {
            Ok(ChunkOutcome::More(progress)) if progress.next == CHUNK_SIZE => {}
            Ok(other) => return Fail(format!("after the first fragment: {other:?}")),
            Err(e) => return Fail(format!("the first fragment: {e}")),
        }
        step!("the edit in OneDrive meanwhile", self.replace(&item.id, &etag, b"edited meanwhile".to_vec(), T0 + 120).await);
        match self.drive.upload_chunk(&session.url, CHUNK_SIZE, total, content[half..].to_vec()).await {
            Ok(ChunkOutcome::Done(done)) => {
                self.note(&done);
                if done.quick_xor_hash() != Some(hash.as_str()) {
                    return Fail(format!("the session completed, but OneDrive has hash {:?}", done.quick_xor_hash()));
                }
                Pass("the last fragment completed over the edit, which is left only in the version history (F80 holds)".into())
            }
            Err(e @ (WriteError::Changed | WriteError::NameExists | WriteError::SessionGone)) => {
                Pass(format!("OneDrive refused the last fragment after the edit ({e}): F80's window does not exist"))
            }
            Ok(ChunkOutcome::More(progress)) => Fail(format!("after the last fragment OneDrive expects {}", progress.next)),
            Err(e) => Fail(format!("the last fragment: {e}")),
        }
    }

    /// §4.7: a delete goes to the recycle bin.
    async fn recycle_bin(&mut self) -> Outcome {
        let name = format!("recycled-{}.txt", self.run_id);
        let content = b"sent to the recycle bin".to_vec();
        let hash = quickxor(&content);
        let item = step!("the file", self.put(&name, content, T0).await);
        let etag = item.e_tag.clone().unwrap_or_default();
        step!("the delete", self.drive.delete_item(&item.id, &etag).await);
        self.gone.insert(item.id.clone());
        match self.drive.item(&item.id).await {
            Err(DriveError::NotFound) => {}
            Ok(_) => return Fail("the file can still be read after its delete".into()),
            Err(e) => return Fail(format!("reading the deleted file: {e}")),
        }
        let restore = step!("the restore", self.api.post(&["me", "drive", "items", &item.id, "restore"], &json!({})).await);
        match restore {
            (200, answer) => {
                let restored: DriveItem = step!("the restored item", serde_json::from_value(answer));
                self.note(&restored);
                if restored.id != item.id || restored.quick_xor_hash() != Some(hash.as_str()) {
                    return Fail(format!("the restore brought back {} with hash {:?}", restored.id, restored.quick_xor_hash()));
                }
                Pass("the deleted file left the drive and came back from the recycle bin, whole".into())
            }
            (401 | 403, _) => Manual(format!(
                "the deleted file left the drive. Graph's restore needs Files.ReadWrite.All, which konedrive does not \
                 ask for: look for {name} in the recycle bin on onedrive.live.com"
            )),
            (status, answer) => Fail(format!("the restore answered {status}: {}", message(&answer))),
        }
    }

    /// The consent check (limitations log F66): the token konedrive hands out after the switch
    /// back to read-only can only read, and OneDrive enforces it.
    async fn read_only_token(&mut self) -> Outcome {
        match self.read_only.create_folder(&self.folder, "read-only-probe").await {
            Err(WriteError::Forbidden) => Pass("a write with it was refused 403".into()),
            Ok(_) => Fail("it made a folder: the switch back to read-only left a token that can write".into()),
            Err(e) => Fail(format!("expected 403, got: {e}")),
        }
    }

    /// §3.7, assumed: the delta feed returns the run's own changes, each with the eTag its write
    /// was answered with, so the reconcile finds nothing to do (echo).
    async fn delta_echo(&mut self) -> Outcome {
        const ATTEMPTS: u32 = 6;
        for attempt in 1..=ATTEMPTS {
            let latest = step!("the delta feed", follow(&self.drive, &self.delta_link).await);
            let mut missing = Vec::new();
            let mut wrong = Vec::new();
            for (id, (name, etag)) in &self.files {
                if self.gone.contains(id) {
                    if latest.get(id).is_some_and(|item| item.deleted.is_none()) {
                        wrong.push(format!("{name}: deleted, but listed as there"));
                    }
                    continue;
                }
                match latest.get(id) {
                    None => missing.push(name.clone()),
                    Some(item) if item.deleted.is_some() => wrong.push(format!("{name}: listed as deleted")),
                    Some(item) if item.e_tag != *etag => wrong.push(format!("{name}: {:?}, its write answered {etag:?}", item.e_tag)),
                    Some(_) => {}
                }
            }
            if missing.is_empty() && wrong.is_empty() {
                let live = self.files.keys().filter(|id| !self.gone.contains(*id)).count();
                return Pass(format!("{live} files came back with the eTags their last write was answered with"));
            }
            if attempt == ATTEMPTS {
                return Fail(format!("not in the feed: {missing:?}; different: {wrong:?}"));
            }
            tokio::time::sleep(self.delta_wait / ATTEMPTS).await;
        }
        unreachable!("the last attempt returns")
    }

    /// Puts the run folder, with whatever is left in it, into the recycle bin.
    pub async fn cleanup(&mut self) -> Outcome {
        self.guard.begin_cleanup();
        let by_hand = format!("delete /{TOP}/{} by hand", self.run_id);
        for _ in 0..2 {
            let folder = match self.drive.item(&self.folder).await {
                Ok(folder) => folder,
                Err(e) => return Fail(format!("reading the run folder: {e}; {by_hand}")),
            };
            let Some(tag) = folder.c_tag.or(folder.e_tag) else { return Fail(format!("the run folder has no tag; {by_hand}")) };
            match self.drive.delete_item(&self.folder, &tag).await {
                Ok(()) => return Pass(format!("/{TOP}/{} is in the recycle bin", self.run_id)),
                Err(WriteError::Changed) => continue,
                Err(e) => return Fail(format!("{e}; {by_hand}")),
            }
        }
        Fail(format!("the run folder kept changing; {by_hand}"))
    }
}

/// The delta feed from `link` to its end, the latest state of each item.
async fn follow(drive: &DriveClient, link: &str) -> Result<HashMap<String, DriveItem>, DriveError> {
    let mut from = DeltaFrom::Link(link.to_owned());
    let mut latest = HashMap::new();
    loop {
        let page = drive.delta(&from).await?;
        for item in page.items {
            latest.insert(item.id.clone(), item);
        }
        match page.next {
            DeltaNext::Page(next) => from = DeltaFrom::Link(next),
            DeltaNext::Done(_) => return Ok(latest),
        }
    }
}

fn message(answer: &Value) -> String {
    answer.pointer("/error/message").and_then(Value::as_str).unwrap_or("no message").to_owned()
}
