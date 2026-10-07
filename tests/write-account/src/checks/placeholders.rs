//! What an upload session's placeholder is in OneDrive (limitations log F172): opening a session
//! for a new file puts an empty file under its name at once, and a session left open (a daemon
//! stopped mid-opening) leaves it there. These checks record what a read of it shows while the
//! session is open — whether it carries what the session request gave it, whether the delta feed
//! or its folder's listing shows it, whether it shows the session's progress — and what frees
//! its name. They answer what the daemon can tell a placeholder by; each ends `LOOK`.

use std::time::Duration;

use konedrive_graph::drive::{ChunkOutcome, WriteError, CHUNK_SIZE};
use serde_json::{json, Value};

use super::{follow, message, show, Outcome, Run, T0};
use Outcome::{Fail, Manual};

/// Every field a placeholder might tell something by.
const SELECT: &str = "$select=id,name,size,eTag,cTag,createdDateTime,lastModifiedDateTime,fileSystemInfo,\
description,createdBy,lastModifiedBy,parentReference,file,pendingOperations";

/// `T0` as Graph writes it.
const T0_ISO: &str = "2023-11-14T22:13:20Z";

/// What a session request marks its file with, to see whether the placeholder shows it.
const DESCRIPTION: &str = "konedrive write test: made on this machine";

/// How long an open session is left alone before its placeholder is read again.
const QUIET: Duration = Duration::from_secs(120);

/// The fields compared between two reads of a placeholder.
const WATCHED: [&str; 8] = ["size", "eTag", "cTag", "lastModifiedDateTime", "fileSystemInfo", "description", "file", "pendingOperations"];

struct Opened {
    url: String,
    /// Whether the session request carried [`DESCRIPTION`] (OneDrive may refuse it).
    described: bool,
}

impl Run {
    /// The placeholder checks alone (`--only placeholders`).
    pub async fn placeholders(&mut self) -> Vec<(&'static str, Outcome)> {
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
        check!("a placeholder while its session is open, and what the session request gave it", self.open_placeholder());
        check!("a placeholder holds its name; what a delete of it does", self.held_name());
        check!("a cancelled session takes its placeholder with it", self.cancelled_session());
        done
    }

    async fn session_request(&self, name: &str, described: bool) -> Result<(u16, Value), String> {
        let mut item = json!({
            "@microsoft.graph.conflictBehavior": "fail",
            "name": name,
            "fileSystemInfo": { "lastModifiedDateTime": T0_ISO },
        });
        if described {
            item["description"] = json!(DESCRIPTION);
        }
        let (parent, child) = (format!("{}:", self.folder), format!("{name}:"));
        self.api.post(&["me", "drive", "items", &parent, &child, "createUploadSession"], &json!({ "item": item })).await
    }

    /// Opens a session for a new file, with a description if OneDrive takes one.
    async fn open(&self, name: &str, notes: &mut Vec<String>) -> Result<Opened, String> {
        let (status, answer) = self.session_request(name, true).await?;
        if status == 200 {
            return Ok(Opened { url: upload_url(&answer)?, described: true });
        }
        notes.push(format!("a session request with a description was answered {status} ({})", message(&answer)));
        let (status, answer) = self.session_request(name, false).await?;
        if status != 200 {
            return Err(format!("the session for {name} was answered {status} ({})", message(&answer)));
        }
        Ok(Opened { url: upload_url(&answer)?, described: false })
    }

    /// What stands at `name` in the run folder, with every field of [`SELECT`].
    async fn read(&self, name: &str, label: &str) -> Result<(u16, Value), String> {
        let parent = format!("{}:", self.folder);
        let (status, mut value) = self.api.get_query(&["me", "drive", "items", &parent, name], SELECT).await?;
        if let Some(object) = value.as_object_mut() {
            object.remove("@odata.context");
        }
        println!("    {label} ({status}): {value}");
        Ok((status, value))
    }

    async fn in_delta(&self, id: &str) -> Result<bool, String> {
        let latest = follow(&self.drive, &self.delta_link).await.map_err(|e| e.to_string())?;
        Ok(latest.contains_key(id))
    }

    async fn open_placeholder(&mut self) -> Outcome {
        let mut notes = Vec::new();
        let name = "open-session.bin";
        let total = 2 * CHUNK_SIZE;
        let opened = step!("opening the session", self.open(name, &mut notes).await);
        let (status, first) = step!("reading the placeholder", self.read(name, "just opened").await);
        if status != 200 {
            return Fail(format!("the placeholder cannot be read by its name: {status} ({})", message(&first)));
        }
        let id = first["id"].as_str().unwrap_or_default().to_owned();
        notes.push(format!("just opened: {}", describe(&first, opened.described)));
        let (status, listing) =
            step!("listing the run folder", self.api.get_query(&["me", "drive", "items", &self.folder, "children"], "$select=id,name,size").await);
        let listed = listing["value"].as_array().is_some_and(|items| items.iter().any(|item| item["id"] == first["id"]));
        notes.push(format!("the folder's listing ({status}) {} it", if listed { "shows" } else { "does not show" }));
        let in_delta = step!("the delta feed", self.in_delta(&id).await);
        notes.push(format!("the delta feed {} it", if in_delta { "lists" } else { "does not list" }));

        let content = self.content(total as usize);
        let half = CHUNK_SIZE as usize;
        match self.drive.upload_chunk(&opened.url, 0, total, content[..half].to_vec()).await {
            Ok(ChunkOutcome::More(_)) => {}
            Ok(other) => return Fail(format!("after the first fragment: {other:?}")),
            Err(e) => return Fail(format!("the first fragment: {e}")),
        }
        let (_, after) = step!("reading it after a fragment", self.read(name, "after 10 MiB of 20").await);
        notes.push(format!("after a fragment: {}", changes(&first, &after)));
        println!("    leaving the session alone for {} s", QUIET.as_secs());
        tokio::time::sleep(QUIET).await;
        let (_, quiet) = step!("reading it after a pause", self.read(name, "after a pause").await);
        notes.push(format!("after {} s with nothing sent: {}", QUIET.as_secs(), changes(&after, &quiet)));
        let in_delta = step!("the delta feed", self.in_delta(&id).await);
        notes.push(format!("the delta feed then {} it", if in_delta { "lists" } else { "does not list" }));

        match self.drive.upload_chunk(&opened.url, CHUNK_SIZE, total, content[half..].to_vec()).await {
            Ok(ChunkOutcome::Done(_)) => {}
            Ok(other) => return Fail(format!("after the last fragment: {other:?}")),
            Err(e) => return Fail(format!("the last fragment: {e}")),
        }
        let (_, done) = step!("reading the complete file", self.read(name, "complete").await);
        notes.push(format!(
            "complete: {} id, {}",
            if done["id"] == first["id"] { "the placeholder's" } else { "another" },
            describe(&done, opened.described)
        ));
        Manual(notes.join("; "))
    }

    async fn held_name(&mut self) -> Outcome {
        let mut notes = Vec::new();
        let name = "held.bin";
        let opened = step!("opening the session", self.open(name, &mut notes).await);
        let (status, holder) = step!("reading the placeholder", self.read(name, "held").await);
        if status != 200 {
            return Fail(format!("the placeholder cannot be read by its name: {status} ({})", message(&holder)));
        }
        let (status, second) = step!("a second session", self.session_request(name, false).await);
        notes.push(format!("a second session for the name: {status} ({})", code(&second)));
        if let (200, Ok(url)) = (status, upload_url(&second)) {
            let _ = self.drive.cancel_upload(&url).await;
        }
        let other = step!("another file", self.put("other.bin", b"other".to_vec(), T0).await);
        match self.rename(&other, name).await {
            Err(WriteError::NameExists) => notes.push("a rename onto the name: 409".into()),
            Ok(renamed) => notes.push(format!("a rename onto the name went through, as {:?}", renamed.name)),
            Err(e) => notes.push(format!("a rename onto the name: {e}")),
        }
        let (id, etag) = (holder["id"].as_str().unwrap_or_default(), holder["eTag"].as_str().unwrap_or_default());
        match self.drive.delete_item(id, etag).await {
            Ok(()) => notes.push("a delete of the placeholder with its eTag: done".into()),
            Err(e) => notes.push(format!("a delete of the placeholder with its eTag: {e}")),
        }
        let (status, _) = step!("reading the name after", self.read(name, "after the delete").await);
        notes.push(format!("the name then reads {status}"));
        match self.drive.upload_status(&opened.url).await {
            Ok(progress) => notes.push(format!("the session then: open, expecting byte {}", progress.next)),
            Err(e) => notes.push(format!("the session then: {e}")),
        }
        let content = self.content(1024);
        match self.drive.upload_chunk(&opened.url, 0, 1024, content).await {
            Ok(ChunkOutcome::Done(item)) => notes.push(format!("a fragment to it then: completed a file {} of {:?} bytes", item.id, item.size)),
            Ok(other) => notes.push(format!("a fragment to it then: {other:?}")),
            Err(e) => notes.push(format!("a fragment to it then: {e}")),
        }
        match self.put(name, b"after".to_vec(), T0).await {
            Ok(_) => notes.push("a new file of the name then: made".into()),
            Err(e) => notes.push(format!("a new file of the name then: {e}")),
        }
        Manual(notes.join("; "))
    }

    async fn cancelled_session(&mut self) -> Outcome {
        let mut notes = Vec::new();
        let name = "cancelled.bin";
        let opened = step!("opening the session", self.open(name, &mut notes).await);
        let (status, _) = step!("reading the placeholder", self.read(name, "before the cancel").await);
        notes.push(format!("before the cancel the name reads {status}"));
        match self.drive.cancel_upload(&opened.url).await {
            Ok(()) => notes.push("the cancel: done".into()),
            Err(e) => return Fail(format!("the cancel: {e}")),
        }
        for attempt in 1..=5 {
            let (status, _) = step!("reading the name", self.read(name, "after the cancel").await);
            if status == 404 || attempt == 5 {
                notes.push(format!("after the cancel the name reads {status} (read {attempt} times, 3 s apart)"));
                break;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        Manual(notes.join("; "))
    }
}

fn upload_url(answer: &Value) -> Result<String, String> {
    answer["uploadUrl"].as_str().map(str::to_owned).ok_or_else(|| "a session answer without an uploadUrl".to_owned())
}

fn code(answer: &Value) -> String {
    answer.pointer("/error/code").and_then(Value::as_str).unwrap_or("no error").to_owned()
}

fn describe(item: &Value, described: bool) -> String {
    let field = |key: &str| item.get(key).map_or("absent".to_owned(), Value::to_string);
    let time = item.pointer("/fileSystemInfo/lastModifiedDateTime").and_then(Value::as_str).unwrap_or("none");
    format!(
        "size {}, file {}, description {}, fileSystemInfo time {time} (sent {T0_ISO}, created {}), createdBy {}, pendingOperations {}",
        field("size"),
        field("file"),
        if described { field("description") } else { "not sent".to_owned() },
        field("createdDateTime"),
        field("createdBy"),
        field("pendingOperations"),
    )
}

fn changes(before: &Value, after: &Value) -> String {
    let changed: Vec<String> = WATCHED
        .iter()
        .filter(|key| before.get(**key) != after.get(**key))
        .map(|key| format!("{key} {} → {}", shown(before.get(*key)), shown(after.get(*key))))
        .collect();
    if changed.is_empty() {
        "nothing changed".to_owned()
    } else {
        changed.join(", ")
    }
}

fn shown(value: Option<&Value>) -> String {
    value.map_or("absent".to_owned(), Value::to_string)
}
