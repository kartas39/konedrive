//! What every request must pass before the proxy forwards it to OneDrive (`docs/design/writes.md` §12,
//! guards 3 and 4). The guard knows three things: which phase the run is in, which items lie
//! inside the run's own folder, and how much of the run's budget is used. A request it refuses
//! never leaves this machine, and after the first refusal it admits nothing but the cleanup.
//!
//! Reads (`GET`) are admitted anywhere: the preflight lists the drive to count its items, and a
//! read changes nothing. Every other request must name, by id, an item inside the run folder, or
//! an item inside it as the parent of something new. The ids inside are learnt from OneDrive's
//! own answers: the run folder from the answer that made it, then every item whose answer names
//! an inside item as its parent.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, PoisonError};

use percent_encoding::percent_decode_str;
use reqwest::Method;
use serde_json::{Map, Value};

pub const MIB: u64 = 1 << 20;

/// The folder at the top of the drive every run makes its own folder in.
pub const TOP: &str = "konedrive-write-test";

/// Requests held back for the cleanup, so that a run that used up its budget can still put its
/// folder into the recycle bin.
pub const CLEANUP_RESERVE: u32 = 10;

/// The run's limits (guard 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// The largest file a request may create, or send a fragment of.
    pub per_file: u64,
    /// Every byte of content the run sends, in all.
    pub per_run: u64,
    /// Every request the run makes, reads and the cleanup's reserve included.
    pub requests: u32,
}

impl Default for Caps {
    fn default() -> Self {
        Caps { per_file: 64 * MIB, per_run: 200 * MIB, requests: 500 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Before guards 1 and 2 have passed: reads only.
    Preflight,
    /// Writes inside the run folder, and, before it exists, the two requests that make it.
    Armed,
    /// Reads, and the run folder's own delete.
    Cleanup,
}

/// Where a request is going, as the proxy received it.
#[derive(Debug, Clone, Copy)]
pub enum Target<'a> {
    /// Graph's API: `rel` is the path below Graph's base, still percent-encoded.
    Graph { rel: &'a str, query: Option<&'a str> },
    /// An upload session's URL the proxy handed out in place of OneDrive's, by its key.
    Upload { key: &'a str },
}

#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub method: &'a Method,
    pub target: Target<'a>,
    pub body: &'a [u8],
    /// The `Content-Range` header of a fragment.
    pub content_range: Option<&'a str>,
}

/// Where an admitted request goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Forward {
    /// To Graph, at the same path below its base.
    Graph,
    /// To the upload session's own URL.
    Upload(String),
}

/// A Graph path, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    /// `me/drive/items/{id}[/{tail}]`
    Item { id: String, tail: Option<String> },
    /// `me/drive/items/{parent}:/{name}[:/{tail}]`
    Child { parent: String, name: String, tail: Option<String> },
    Other,
}

fn route(rel: &str) -> Route {
    let decoded: Vec<String> =
        rel.split('/').map(|segment| percent_decode_str(segment).decode_utf8_lossy().into_owned()).collect();
    let segments: Vec<&str> = decoded.iter().map(String::as_str).collect();
    let ["me", "drive", "items", first, rest @ ..] = segments.as_slice() else { return Route::Other };
    let owned = |s: &str| s.to_owned();
    match first.strip_suffix(':') {
        Some(parent) => match rest {
            [name] if !name.ends_with(':') => Route::Child { parent: owned(parent), name: owned(name), tail: None },
            [name, tail] => match name.strip_suffix(':') {
                Some(name) => Route::Child { parent: owned(parent), name: owned(name), tail: Some(owned(tail)) },
                None => Route::Other,
            },
            _ => Route::Other,
        },
        None => match rest {
            [] => Route::Item { id: owned(first), tail: None },
            [tail] => Route::Item { id: owned(first), tail: Some(owned(tail)) },
            _ => Route::Other,
        },
    }
}

struct Session {
    url: String,
    size: u64,
}

struct State {
    phase: Phase,
    requests: u32,
    bytes: u64,
    root: Option<String>,
    top: Option<String>,
    run_folder: Option<String>,
    inside: HashSet<String>,
    sessions: HashMap<String, Session>,
    /// The first refusal: after it nothing is admitted but the cleanup.
    refused: Option<String>,
}

impl State {
    fn inside(&self, id: &str) -> bool {
        self.inside.contains(id)
    }
}

pub struct Guard {
    caps: Caps,
    run_id: String,
    state: Mutex<State>,
}

impl Guard {
    pub fn new(caps: Caps, run_id: String) -> Guard {
        let state = State {
            phase: Phase::Preflight,
            requests: 0,
            bytes: 0,
            root: None,
            top: None,
            run_folder: None,
            inside: HashSet::new(),
            sessions: HashMap::new(),
            refused: None,
        };
        Guard { caps, run_id, state: Mutex::new(state) }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Guards 1 and 2 have passed: writes may begin.
    pub fn arm(&self) {
        let mut state = self.state();
        if state.phase == Phase::Preflight {
            state.phase = Phase::Armed;
        }
    }

    /// The checks are over, or were cut short: from now on only reads and the run folder's own
    /// delete are admitted, with the requests held back for them.
    pub fn begin_cleanup(&self) {
        self.state().phase = Phase::Cleanup;
    }

    /// Why the guard refused a request, if it did.
    pub fn refused(&self) -> Option<String> {
        self.state().refused.clone()
    }

    /// Records a refusal made before [`admit`](Self::admit) was asked (a path the proxy does
    /// not serve): it counts as the guard's own.
    pub fn record_refusal(&self, why: &str) {
        let mut state = self.state();
        if state.refused.is_none() {
            state.refused = Some(why.to_owned());
        }
    }

    pub fn run_folder(&self) -> Option<String> {
        self.state().run_folder.clone()
    }

    #[cfg(test)]
    pub fn is_inside(&self, id: &str) -> bool {
        self.state().inside(id)
    }

    /// Requests made and content bytes sent so far.
    pub fn usage(&self) -> (u32, u64) {
        let state = self.state();
        (state.requests, state.bytes)
    }

    /// Registers the upload URL OneDrive answered a session request with, for a file of `size`
    /// bytes, and answers the key the proxy hands out in its place.
    pub fn open_session(&self, url: String, size: u64) -> String {
        let mut state = self.state();
        let key = format!("s{}", state.sessions.len() + 1);
        state.sessions.insert(key.clone(), Session { url, size });
        key
    }

    /// Decides one request, before anything of it is sent. A refusal is final for the run:
    /// only the cleanup is admitted after it.
    pub fn admit(&self, request: &Request<'_>) -> Result<Forward, String> {
        let mut state = self.state();
        let verdict = self.decide(&mut state, request);
        match &verdict {
            Ok(_) => state.requests += 1,
            Err(why) => {
                if state.refused.is_none() {
                    state.refused = Some(why.clone());
                }
            }
        }
        verdict
    }

    fn decide(&self, state: &mut State, request: &Request<'_>) -> Result<Forward, String> {
        let cleanup = state.phase == Phase::Cleanup;
        if let Some(first) = state.refused.as_ref().filter(|_| !cleanup) {
            return Err(format!("the guard has already refused a request ({first}); nothing more is sent"));
        }
        let limit = if cleanup { self.caps.requests } else { self.caps.requests.saturating_sub(CLEANUP_RESERVE) };
        if state.requests >= limit {
            return Err(format!("the run's budget of {limit} requests is used up"));
        }
        match request.target {
            Target::Upload { key } => self.upload(state, request, key),
            Target::Graph { rel, query } => self.graph(state, request, rel, query),
        }
    }

    fn upload(&self, state: &mut State, request: &Request<'_>, key: &str) -> Result<Forward, String> {
        let Some(session) = state.sessions.get(key) else {
            return Err("an upload URL this run was not given".into());
        };
        let (url, size) = (session.url.clone(), session.size);
        match *request.method {
            Method::GET | Method::DELETE if state.phase == Phase::Armed => Ok(Forward::Upload(url)),
            Method::PUT if state.phase == Phase::Armed => {
                let (first, last, total) = request
                    .content_range
                    .and_then(content_range)
                    .ok_or("a fragment without a readable Content-Range")?;
                if total != size || last >= total || last + 1 - first != request.body.len() as u64 {
                    return Err(format!(
                        "a fragment of bytes {first}-{last}/{total}, {} bytes long, does not fit its session of {size} bytes",
                        request.body.len()
                    ));
                }
                self.content(state, request.body.len() as u64)?;
                Ok(Forward::Upload(url))
            }
            ref method => Err(format!("{method} to an upload URL is not a request this run makes now")),
        }
    }

    /// Counts `bytes` of content against the caps.
    fn content(&self, state: &mut State, bytes: u64) -> Result<(), String> {
        if bytes > self.caps.per_file {
            return Err(format!("{bytes} bytes in one request is over the cap of {} per file", self.caps.per_file));
        }
        if state.bytes + bytes > self.caps.per_run {
            return Err(format!(
                "{bytes} more bytes would take the run past its cap of {} ({} sent so far)",
                self.caps.per_run, state.bytes
            ));
        }
        state.bytes += bytes;
        Ok(())
    }

    fn graph(&self, state: &mut State, request: &Request<'_>, rel: &str, query: Option<&str>) -> Result<Forward, String> {
        let method = request.method;
        if *method == Method::GET {
            return Ok(Forward::Graph);
        }
        let what = format!("{method} {rel}");
        let route = route(rel);
        match state.phase {
            Phase::Preflight => return Err(format!("{what} before the preflight's guards passed")),
            Phase::Cleanup => {
                return match (&route, state.run_folder.as_deref()) {
                    (Route::Item { id, tail: None }, Some(run)) if *method == Method::DELETE && id == run => {
                        Ok(Forward::Graph)
                    }
                    _ => Err(format!("{what}: during the cleanup only the run folder's own delete is sent")),
                };
            }
            Phase::Armed => {}
        }
        let Some(run) = state.run_folder.clone() else {
            return self.bootstrap(state, method, &route, request.body, &what);
        };
        let outside = |id: &str| {
            if id == run {
                format!("{what}: the run folder itself is changed only by the cleanup's delete")
            } else {
                format!("{what}: {id} is not inside the run folder /{TOP}/{}", self.run_id)
            }
        };
        match (method, &route) {
            (&Method::POST, Route::Item { id, tail: Some(tail) }) if tail == "children" => {
                if !state.inside(id) {
                    return Err(outside(id));
                }
                let body = object(request.body, &what)?;
                check_name(body.get("name").and_then(Value::as_str).unwrap_or_default(), &what)?;
                if !body.get("folder").is_some_and(Value::is_object) || !fails_on_conflict(&body) {
                    return Err(format!("{what}: only a folder, with conflictBehavior fail, is made this way"));
                }
                Ok(Forward::Graph)
            }
            (&Method::POST, Route::Item { id, tail: Some(tail) }) if tail == "createUploadSession" => {
                if !state.inside(id) || *id == run {
                    return Err(outside(id));
                }
                self.session_item(request.body, &what)?;
                Ok(Forward::Graph)
            }
            (&Method::POST, Route::Child { parent, name, tail: Some(tail) }) if tail == "createUploadSession" => {
                if !state.inside(parent) {
                    return Err(outside(parent));
                }
                check_name(name, &what)?;
                let item = self.session_item(request.body, &what)?;
                let other_name = item.get("name").is_some_and(|given| given.as_str() != Some(name.as_str()));
                if !fails_on_conflict(&item) || other_name {
                    return Err(format!("{what}: a new file's session must carry conflictBehavior fail and its own name"));
                }
                Ok(Forward::Graph)
            }
            (&Method::POST, Route::Item { id, tail: Some(tail) }) if tail == "restore" => {
                if !state.inside(id) {
                    return Err(outside(id));
                }
                for (key, value) in object(request.body, &what)? {
                    match key.as_str() {
                        "name" => check_name(value.as_str().unwrap_or_default(), &what)?,
                        "parentReference" => parent_inside(state, &value, &what)?,
                        other => return Err(format!("{what}: {other} is not something a restore here sends")),
                    }
                }
                Ok(Forward::Graph)
            }
            (&Method::PUT, Route::Item { id, tail: Some(tail) }) if tail == "content" => {
                if !state.inside(id) || *id == run {
                    return Err(outside(id));
                }
                self.content(state, request.body.len() as u64)?;
                Ok(Forward::Graph)
            }
            (&Method::PUT, Route::Child { parent, name, tail: Some(tail) }) if tail == "content" => {
                if !state.inside(parent) {
                    return Err(outside(parent));
                }
                check_name(name, &what)?;
                let fails = url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
                    .any(|(key, value)| key == "@microsoft.graph.conflictBehavior" && value == "fail");
                if !fails {
                    return Err(format!("{what}: a new file's PUT must carry conflictBehavior=fail in its URL"));
                }
                self.content(state, request.body.len() as u64)?;
                Ok(Forward::Graph)
            }
            (&Method::PATCH, Route::Item { id, tail: None }) => {
                if !state.inside(id) || *id == run {
                    return Err(outside(id));
                }
                for (key, value) in object(request.body, &what)? {
                    match key.as_str() {
                        "name" => check_name(value.as_str().unwrap_or_default(), &what)?,
                        "parentReference" => parent_inside(state, &value, &what)?,
                        "fileSystemInfo" => {}
                        other => return Err(format!("{what}: {other} is not something a change here sends")),
                    }
                }
                Ok(Forward::Graph)
            }
            (&Method::DELETE, Route::Item { id, tail: None }) => {
                if !state.inside(id) || *id == run {
                    return Err(outside(id));
                }
                Ok(Forward::Graph)
            }
            _ => Err(format!("{what} is not a request this harness makes")),
        }
    }

    /// Before the run folder exists, only the top folder (in the drive's root) and the run
    /// folder (in the top folder) may be made, each exactly as the harness asks for it.
    fn bootstrap(&self, state: &State, method: &Method, route: &Route, body: &[u8], what: &str) -> Result<Forward, String> {
        let refused = || format!("{what}: nothing but the run folder may be made before it exists");
        let Route::Item { id, tail: Some(tail) } = route else { return Err(refused()) };
        let expected = if Some(id) == state.root.as_ref() {
            TOP
        } else if Some(id) == state.top.as_ref() {
            self.run_id.as_str()
        } else {
            return Err(refused());
        };
        let body = object(body, what)?;
        let exact = *method == Method::POST
            && tail == "children"
            && body.len() == 3
            && body.get("name").and_then(Value::as_str) == Some(expected)
            && body.get("folder").is_some_and(Value::is_object)
            && fails_on_conflict(&body);
        if !exact {
            return Err(format!("{what}: only the folder {expected}, with conflictBehavior fail, is made here"));
        }
        Ok(Forward::Graph)
    }

    /// A session request's `item`, whose `fileSize` must be given and within the cap.
    fn session_item(&self, body: &[u8], what: &str) -> Result<Map<String, Value>, String> {
        let mut body = object(body, what)?;
        let item = match body.remove("item") {
            Some(Value::Object(item)) if body.is_empty() => item,
            _ => return Err(format!("{what}: a session request is one item")),
        };
        let size = item.get("fileSize").and_then(Value::as_u64).ok_or_else(|| format!("{what}: no fileSize"))?;
        if size > self.caps.per_file {
            return Err(format!("{what}: a file of {size} bytes is over the cap of {} per file", self.caps.per_file));
        }
        Ok(item)
    }

    /// Learns from an answer OneDrive gave: the drive's root, the top folder, the run folder
    /// (only from the answer to the request that made it), and the items inside.
    pub fn learn(&self, method: &Method, rel: Option<&str>, status: u16, answer: &Value) {
        if !(200..300).contains(&status) {
            return;
        }
        let mut state = self.state();
        if let (Some(rel), Some(top)) = (rel, state.top.clone()) {
            let made_here = *method == Method::POST
                && status == 201
                && state.run_folder.is_none()
                && route(rel) == Route::Item { id: top, tail: Some("children".into()) }
                && answer.get("name").and_then(Value::as_str) == Some(self.run_id.as_str());
            if let Some(id) = answer.get("id").and_then(Value::as_str).filter(|_| made_here) {
                state.run_folder = Some(id.to_owned());
                state.inside.insert(id.to_owned());
            }
        }
        let items: Vec<&Value> = match answer.get("value").and_then(Value::as_array) {
            Some(values) => values.iter().collect(),
            None => vec![answer],
        };
        for item in items {
            let Some(id) = item.get("id").and_then(Value::as_str) else { continue };
            if item.get("root").is_some_and(Value::is_object) {
                state.root = Some(id.to_owned());
            }
            let parent = item.pointer("/parentReference/id").and_then(Value::as_str);
            let is_top = item.get("folder").is_some_and(Value::is_object)
                && item.get("name").and_then(Value::as_str) == Some(TOP)
                && parent.is_some()
                && parent == state.root.as_deref();
            if is_top {
                state.top = Some(id.to_owned());
            }
            if parent.is_some_and(|parent| state.inside(parent)) {
                state.inside.insert(id.to_owned());
            }
        }
    }
}

/// The `fileSize` a session request declares.
pub fn declared_size(body: &[u8]) -> Option<u64> {
    serde_json::from_slice::<Value>(body).ok()?.pointer("/item/fileSize")?.as_u64()
}

fn object(body: &[u8], what: &str) -> Result<Map<String, Value>, String> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(format!("{what}: the body is not a JSON object")),
    }
}

fn fails_on_conflict(body: &Map<String, Value>) -> bool {
    body.get("@microsoft.graph.conflictBehavior").and_then(Value::as_str) == Some("fail")
}

/// `{"id": …}` naming an item inside the run folder.
fn parent_inside(state: &State, value: &Value, what: &str) -> Result<(), String> {
    let parent = value.as_object().filter(|reference| reference.len() == 1).and_then(|reference| reference.get("id"));
    match parent.and_then(Value::as_str) {
        Some(id) if state.inside(id) => Ok(()),
        Some(id) => Err(format!("{what}: the new parent {id} is not inside the run folder")),
        None => Err(format!("{what}: a parent is named by its id alone")),
    }
}

/// A name the harness gives: one plain name, never a path.
fn check_name(name: &str, what: &str) -> Result<(), String> {
    let plain = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', ':'])
        && !name.chars().any(char::is_control);
    if plain {
        Ok(())
    } else {
        Err(format!("{what}: {name:?} is not one plain name"))
    }
}

/// `bytes <first>-<last>/<total>`.
fn content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    let (first, last, total): (u64, u64, u64) =
        (first.trim().parse().ok()?, last.trim().parse().ok()?, total.trim().parse().ok()?);
    (first <= last).then_some((first, last, total))
}
