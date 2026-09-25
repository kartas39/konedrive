//! A fake OneDrive for the worker's tests and the VM suite's write scenarios (built with
//! `fault-injection`): an in-memory drive behind a
//! wiremock server, answering the requests `DriveClient` makes as Graph
//! documents them — `If-Match` on eTag or cTag, `409` on a name taken
//! (without case), a folder's cTag changing with anything below it, deletes
//! to a recycle bin, upload sessions with `Content-Range`. Never a real
//! network.

use std::collections::{BTreeMap, HashMap};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
#[cfg(test)]
use tokio_util::sync::CancellationToken;
use url::Url;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

#[cfg(test)]
use super::{Engine, Limits, OutboxHost, WorkerConfig};
use crate::drive::item::format_graph_time;
use crate::drive::{DriveClient, RetryPolicy};
use crate::quickxor::QuickXor;
#[cfg(test)]
use crate::sync::root::SyncRoot;
#[cfg(test)]
use crate::sync::InodeLocks;
use crate::token::StaticToken;
#[cfg(test)]
use crate::tree::{ActivityRow, Kind, Store};

pub const ROOT: &str = "R";

#[derive(Debug, Clone)]
pub struct FakeItem {
    pub id: String,
    pub parent: Option<String>,
    pub name: String,
    pub folder: bool,
    pub content: Vec<u8>,
    pub hash: Option<String>,
    pub size: u64,
    pub etag: String,
    pub ctag: String,
    pub mtime: i64,
}

enum Target {
    New { parent: String, name: String },
    Existing { id: String },
}

struct Session {
    target: Target,
    size: u64,
    data: Vec<u8>,
    mtime: i64,
}

pub fn qx(content: &[u8]) -> String {
    let mut hasher = QuickXor::new();
    hasher.update(content);
    hasher.finish_base64()
}

fn error(status: u16, code: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({ "error": { "code": code, "message": code } }))
}

/// `%XX` decoded.
fn decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&segment[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Default)]
pub struct Cloud {
    pub items: BTreeMap<String, FakeItem>,
    /// OneDrive's recycle bin.
    pub bin: BTreeMap<String, FakeItem>,
    sessions: HashMap<String, Session>,
    counter: u64,
    base: String,
    /// Scripted answers: (method, a fragment of the path, the answer, how many times).
    script: Vec<(String, String, ResponseTemplate, u32)>,
    /// Every request: method and decoded path below the server.
    pub log: Vec<(String, String)>,
    /// The `If-Match` of every request that had one: path, tag.
    pub guards: Vec<(String, String)>,
    /// Every item that changed, in order: the delta feed's cursor is a
    /// position in it (the read-write reconcile), as Graph's is.
    changes: Vec<String>,
    /// Each delta answers the whole drive again, whatever its token: a feed
    /// that repeats what did not change (an opt-in; Graph may do it).
    pub full_deltas: bool,
    /// Throttled requests: (method, a fragment of the path, how many to let
    /// through first, `Retry-After` seconds, how many to throttle).
    throttles: Vec<(String, String, u32, u32, u32)>,
}

impl Cloud {
    fn tag(&mut self, prefix: &str, id: &str) -> String {
        self.counter += 1;
        self.changes.push(id.to_owned());
        format!("{prefix}-{id}-v{}", self.counter)
    }

    pub fn add(&mut self, item: FakeItem) {
        self.changes.push(item.id.clone());
        self.items.insert(item.id.clone(), item);
    }

    /// Item `id`, and everything below it, deleted in OneDrive (another
    /// device): to the recycle bin.
    pub fn trash(&mut self, id: &str) {
        let parent = self.items.get(id).and_then(|i| i.parent.clone());
        let mut gone = self.below(id);
        gone.push(id.to_owned());
        for id in gone {
            if let Some(item) = self.items.remove(&id) {
                self.changes.push(id.clone());
                self.bin.insert(id, item);
            }
        }
        self.touch_above(parent);
    }

    pub fn add_file(&mut self, id: &str, parent: &str, name: &str, content: &[u8]) {
        self.add(FakeItem {
            id: id.into(),
            parent: Some(parent.into()),
            name: name.into(),
            folder: false,
            content: content.to_vec(),
            hash: Some(qx(content)),
            size: content.len() as u64,
            etag: format!("e-{id}"),
            ctag: format!("c-{id}"),
            mtime: 0,
        });
    }

    pub fn item(&self, id: &str) -> Option<&FakeItem> {
        self.items.get(id)
    }

    /// Where item `id` is, `a/b/c` below the root.
    pub fn path_of(&self, id: &str) -> Option<String> {
        let mut parts = Vec::new();
        let mut at = self.items.get(id)?;
        while let Some(parent) = &at.parent {
            parts.push(at.name.clone());
            at = self.items.get(parent)?;
        }
        parts.reverse();
        Some(parts.join("/"))
    }

    /// The item at `path` (`a/b/c`), names compared exactly.
    pub fn at(&self, path: &str) -> Option<&FakeItem> {
        self.items.values().find(|i| self.path_of(&i.id).as_deref() == Some(path))
    }

    /// Every path in the drive, sorted.
    pub fn paths(&self) -> Vec<String> {
        let mut out: Vec<String> = self.items.keys().filter(|id| id.as_str() != ROOT).filter_map(|id| self.path_of(id)).collect();
        out.sort();
        out
    }

    fn child_named(&self, parent: &str, name: &str, except: Option<&str>) -> Option<&FakeItem> {
        let lower = name.to_lowercase();
        self.items.values().find(|i| i.parent.as_deref() == Some(parent) && i.name.to_lowercase() == lower && Some(i.id.as_str()) != except)
    }

    /// A change inside a folder changes its cTag, and its parents' (§3.6).
    fn touch_above(&mut self, parent: Option<String>) {
        let mut at = parent;
        while let Some(id) = at {
            let ctag = self.tag("c", &id);
            let Some(folder) = self.items.get_mut(&id) else { break };
            folder.ctag = ctag;
            at = folder.parent.clone();
        }
    }

    /// Something changed below folder `id` in OneDrive: its cTag moves.
    pub fn touch(&mut self, id: &str) {
        self.touch_above(Some(id.into()));
    }

    /// An edit made in OneDrive (another device).
    pub fn edit(&mut self, id: &str, content: &[u8]) {
        let (etag, ctag) = (self.tag("e", id), self.tag("c", id));
        let item = self.items.get_mut(id).expect("no such item");
        item.content = content.to_vec();
        item.hash = Some(qx(content));
        item.size = content.len() as u64;
        item.etag = etag;
        item.ctag = ctag;
        let parent = item.parent.clone();
        self.touch_above(parent);
    }

    /// A rename or move made in OneDrive.
    pub fn rename(&mut self, id: &str, parent: &str, name: &str) {
        let etag = self.tag("e", id);
        let old = self.items[id].parent.clone();
        let item = self.items.get_mut(id).expect("no such item");
        item.name = name.into();
        item.parent = Some(parent.into());
        item.etag = etag;
        self.touch_above(old);
        self.touch_above(Some(parent.into()));
    }

    /// Requests of `method` whose path holds `fragment` are throttled — `503`
    /// with `Retry-After: seconds` — once `pass` of them went through, `times`
    /// times: the VM suite's way of holding an upload session half sent.
    pub fn throttle(&mut self, method: &str, fragment: &str, pass: u32, seconds: u32, times: u32) {
        self.throttles.push((method.into(), fragment.into(), pass, seconds, times));
    }

    /// The next `times` requests of `method` whose path holds `fragment`
    /// get `answer`.
    pub fn script(&mut self, method: &str, fragment: &str, answer: ResponseTemplate, times: u32) {
        self.script.push((method.into(), fragment.into(), answer, times));
    }

    pub fn count(&self, method: &str, fragment: &str) -> usize {
        self.log.iter().filter(|(m, p)| m == method && p.contains(fragment)).count()
    }

    /// The delta feed as the drive is now (the read-write reconcile's cycles): every item, and
    /// what went to the recycle bin as deleted — a feed may repeat what did
    /// not change, and the store takes an entry for what it holds already as
    /// no change. One page, ending with a delta link.
    pub fn delta_body(&self) -> Value {
        let mut value: Vec<Value> = self.items.values().map(|item| self.json(item)).collect();
        value.extend(self.bin.values().map(|item| json!({ "id": item.id, "deleted": {}, "parentReference": { "id": item.parent } })));
        json!({ "value": value, "@odata.deltaLink": self.delta_link() })
    }

    /// The link a delta hands out now: the cursor at the latest change.
    fn delta_link(&self) -> String {
        format!("{}/me/drive/root/delta?token=t{}", self.base, self.changes.len())
    }

    /// What changed since cursor `from`, each item once, as it is now — a
    /// deleted one as deleted — with the link to go on from.
    pub fn delta_since(&self, from: usize) -> Value {
        let mut seen = std::collections::HashSet::new();
        let mut value = Vec::new();
        for id in self.changes.iter().skip(from) {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(item) = self.items.get(id) {
                value.push(self.json(item));
            } else if let Some(item) = self.bin.get(id) {
                value.push(json!({ "id": item.id, "deleted": {}, "parentReference": { "id": item.parent } }));
            }
        }
        json!({ "value": value, "@odata.deltaLink": self.delta_link() })
    }

    fn json(&self, item: &FakeItem) -> Value {
        let mut value = json!({
            "id": item.id,
            "name": item.name,
            "eTag": item.etag,
            "cTag": item.ctag,
            "size": item.size,
            "fileSystemInfo": { "lastModifiedDateTime": format_graph_time(item.mtime) },
        });
        match &item.parent {
            Some(parent) => value["parentReference"] = json!({ "id": parent, "driveId": "D" }),
            None => value["root"] = json!({}),
        }
        if item.folder {
            let children = self.items.values().filter(|i| i.parent.as_deref() == Some(item.id.as_str())).count();
            value["folder"] = json!({ "childCount": children });
        } else {
            value["file"] = json!({ "mimeType": "application/octet-stream", "hashes": { "quickXorHash": item.hash } });
            value["@microsoft.graph.downloadUrl"] = json!(format!("{}/dl/{}", self.base, item.id));
        }
        value
    }

    fn answer(&self, status: u16, id: &str) -> ResponseTemplate {
        ResponseTemplate::new(status).set_body_json(self.json(&self.items[id]))
    }

    fn guard(item: &FakeItem, if_match: Option<&str>) -> bool {
        if_match.is_none_or(|tag| tag == item.etag || tag == item.ctag)
    }

    fn below(&self, id: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![id.to_owned()];
        while let Some(at) = stack.pop() {
            for child in self.items.values().filter(|i| i.parent.as_deref() == Some(at.as_str())) {
                out.push(child.id.clone());
                stack.push(child.id.clone());
            }
        }
        out
    }

    fn new_id(&mut self) -> String {
        self.counter += 1;
        format!("N{}", self.counter)
    }

    fn handle(&mut self, request: &Request) -> ResponseTemplate {
        let method = request.method.to_string();
        let segments: Vec<String> = request.url.path_segments().map(|s| s.map(decode).collect()).unwrap_or_default();
        let joined = segments.join("/");
        self.log.push((method.clone(), joined.clone()));
        if let Some(throttle) = self.throttles.iter_mut().find(|(m, f, _, _, left)| *m == method && joined.contains(f.as_str()) && *left > 0) {
            if throttle.2 > 0 {
                throttle.2 -= 1;
            } else {
                throttle.4 -= 1;
                return error(503, "serviceNotAvailable").insert_header("Retry-After", throttle.3.to_string().as_str());
            }
        }
        if let Some(scripted) = self.script.iter_mut().find(|(m, f, _, left)| *m == method && joined.contains(f.as_str()) && *left > 0) {
            scripted.3 -= 1;
            return scripted.2.clone();
        }
        let if_match = request.headers.get("if-match").and_then(|v| v.to_str().ok()).map(str::to_owned);
        if let Some(tag) = &if_match {
            self.guards.push((joined.clone(), tag.clone()));
        }
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let s: Vec<&str> = segments.iter().map(String::as_str).collect();
        match (method.as_str(), s.as_slice()) {
            ("GET", ["me", "drive", "items", parent, name]) if parent.ends_with(':') => self.child(parent.trim_end_matches(':'), name),
            ("POST", ["me", "drive", "items", parent, name, "createUploadSession"]) if parent.ends_with(':') => {
                self.session_new(parent.trim_end_matches(':'), name.trim_end_matches(':'), &body)
            }
            ("PUT", ["me", "drive", "items", parent, name, "content"]) if parent.ends_with(':') => {
                self.put_new(parent.trim_end_matches(':'), name.trim_end_matches(':'))
            }
            ("GET", ["me", "drive", "items", id]) => match self.items.contains_key(*id) {
                true => self.answer(200, id),
                false => error(404, "itemNotFound"),
            },
            ("PATCH", ["me", "drive", "items", id]) => self.patch(id, if_match.as_deref(), &body),
            ("DELETE", ["me", "drive", "items", id]) => self.delete(id, if_match.as_deref()),
            ("GET", ["me", "drive", "items", id, "children"]) => {
                let value: Vec<Value> = self.items.values().filter(|i| i.parent.as_deref() == Some(*id)).map(|i| self.json(i)).collect();
                ResponseTemplate::new(200).set_body_json(json!({ "value": value }))
            }
            ("POST", ["me", "drive", "items", id, "children"]) => self.mkdir(id, &body),
            ("POST", ["me", "drive", "items", id, "createUploadSession"]) => self.session_existing(id, if_match.as_deref(), &body),
            ("PUT", ["me", "drive", "items", id, "content"]) => self.put_existing(id, if_match.as_deref()),
            ("PUT", ["upload", sid]) => {
                let range = request.headers.get("content-range").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
                self.fragment(sid, &range, &request.body)
            }
            ("GET", ["upload", sid]) => match self.sessions.get(*sid) {
                Some(session) => ResponseTemplate::new(200).set_body_json(json!({
                    "nextExpectedRanges": [format!("{}-", session.data.len())],
                    "expirationDateTime": "2099-01-01T00:00:00Z",
                })),
                None => error(404, "itemNotFound"),
            },
            ("DELETE", ["upload", sid]) => {
                self.sessions.remove(*sid);
                ResponseTemplate::new(204)
            }
            // the read-write reconcile's cycles: the account's drive, the delta feed, the bytes.
            ("GET", ["me", "drive"]) => ResponseTemplate::new(200).set_body_json(json!({ "id": "D" })),
            ("GET", ["me", "drive", "root", "delta"]) => {
                let from = request.url.query_pairs().find(|(k, _)| k == "token").and_then(|(_, v)| v.strip_prefix('t').and_then(|n| n.parse::<usize>().ok()));
                let body = match from {
                    Some(from) if !self.full_deltas => self.delta_since(from),
                    _ => self.delta_body(),
                };
                ResponseTemplate::new(200).set_body_json(body)
            }
            ("GET", ["dl", id]) => match self.items.get(*id) {
                Some(item) => ResponseTemplate::new(200).set_body_bytes(item.content.clone()),
                None => error(404, "itemNotFound"),
            },
            _ => error(400, "invalidRequest"),
        }
    }

    fn child(&self, parent: &str, name: &str) -> ResponseTemplate {
        match self.child_named(parent, name, None) {
            Some(item) => ResponseTemplate::new(200).set_body_json(self.json(item)),
            None => error(404, "itemNotFound"),
        }
    }

    fn mkdir(&mut self, parent: &str, body: &Value) -> ResponseTemplate {
        if !self.items.get(parent).is_some_and(|p| p.folder) {
            return error(404, "itemNotFound");
        }
        let name = body["name"].as_str().unwrap_or_default().to_owned();
        if self.child_named(parent, &name, None).is_some() {
            return error(409, "nameAlreadyExists");
        }
        let id = self.new_id();
        let (etag, ctag) = (self.tag("e", &id), self.tag("c", &id));
        self.add(FakeItem { id: id.clone(), parent: Some(parent.into()), name, folder: true, content: Vec::new(), hash: None, size: 0, etag, ctag, mtime: 0 });
        self.touch_above(Some(parent.into()));
        self.answer(201, &id)
    }

    fn patch(&mut self, id: &str, if_match: Option<&str>, body: &Value) -> ResponseTemplate {
        let Some(item) = self.items.get(id).cloned() else { return error(404, "itemNotFound") };
        if !Self::guard(&item, if_match) {
            return error(412, "preconditionFailed");
        }
        let parent = body["parentReference"]["id"].as_str().map(str::to_owned).or(item.parent.clone()).unwrap_or_default();
        let name = body["name"].as_str().map(str::to_owned).unwrap_or(item.name.clone());
        if !self.items.get(&parent).is_some_and(|p| p.folder) {
            return error(404, "itemNotFound");
        }
        if parent == id || self.below(id).contains(&parent) {
            return error(400, "invalidRequest");
        }
        if self.child_named(&parent, &name, Some(id)).is_some() {
            return error(409, "nameAlreadyExists");
        }
        let etag = self.tag("e", id);
        let moved = Some(&parent) != item.parent.as_ref() || name != item.name;
        {
            let entry = self.items.get_mut(id).expect("checked");
            entry.parent = Some(parent.clone());
            entry.name = name;
            entry.etag = etag;
            if let Some(time) = body["fileSystemInfo"]["lastModifiedDateTime"].as_str() {
                entry.mtime = crate::drive::item::parse_graph_time(time).unwrap_or(0);
            }
        }
        if moved {
            self.touch_above(item.parent.clone());
            self.touch_above(Some(parent));
        }
        self.answer(200, id)
    }

    fn delete(&mut self, id: &str, if_match: Option<&str>) -> ResponseTemplate {
        let Some(item) = self.items.get(id).cloned() else { return error(404, "itemNotFound") };
        if !Self::guard(&item, if_match) {
            return error(412, "preconditionFailed");
        }
        for below in self.below(id) {
            if let Some(gone) = self.items.remove(&below) {
                self.changes.push(below.clone());
                self.bin.insert(below, gone);
            }
        }
        self.items.remove(id);
        self.changes.push(id.to_owned());
        self.bin.insert(id.into(), item.clone());
        self.touch_above(item.parent);
        ResponseTemplate::new(204)
    }

    fn open_session(&mut self, target: Target, body: &Value) -> ResponseTemplate {
        let size = body["item"]["fileSize"].as_u64().unwrap_or(0);
        let mtime = body["item"]["fileSystemInfo"]["lastModifiedDateTime"].as_str().and_then(crate::drive::item::parse_graph_time).unwrap_or(0);
        self.counter += 1;
        let sid = format!("s{}", self.counter);
        self.sessions.insert(sid.clone(), Session { target, size, data: Vec::new(), mtime });
        ResponseTemplate::new(200).set_body_json(json!({
            "uploadUrl": format!("{}/upload/{sid}", self.base),
            "expirationDateTime": "2099-01-01T00:00:00Z",
        }))
    }

    fn session_new(&mut self, parent: &str, name: &str, body: &Value) -> ResponseTemplate {
        if !self.items.get(parent).is_some_and(|p| p.folder) {
            return error(404, "itemNotFound");
        }
        if self.child_named(parent, name, None).is_some() {
            return error(409, "nameAlreadyExists");
        }
        self.open_session(Target::New { parent: parent.into(), name: name.into() }, body)
    }

    fn session_existing(&mut self, id: &str, if_match: Option<&str>, body: &Value) -> ResponseTemplate {
        let Some(item) = self.items.get(id) else { return error(404, "itemNotFound") };
        if !Self::guard(item, if_match) {
            return error(412, "preconditionFailed");
        }
        self.open_session(Target::Existing { id: id.into() }, body)
    }

    /// Writes `content` as the file the target names; the item's id.
    #[allow(clippy::result_large_err)]
    fn land(&mut self, target: &Target, content: Vec<u8>, mtime: i64) -> Result<(u16, String), ResponseTemplate> {
        match target {
            Target::New { parent, name } => {
                if !self.items.contains_key(parent) {
                    return Err(error(404, "itemNotFound"));
                }
                if self.child_named(parent, name, None).is_some() {
                    return Err(error(409, "nameAlreadyExists"));
                }
                let id = self.new_id();
                let (etag, ctag) = (self.tag("e", &id), self.tag("c", &id));
                self.add(FakeItem {
                    id: id.clone(),
                    parent: Some(parent.clone()),
                    name: name.clone(),
                    folder: false,
                    hash: Some(qx(&content)),
                    size: content.len() as u64,
                    content,
                    etag,
                    ctag,
                    mtime,
                });
                self.touch_above(Some(parent.clone()));
                Ok((201, id))
            }
            Target::Existing { id } => {
                if !self.items.contains_key(id) {
                    return Err(error(404, "itemNotFound"));
                }
                self.edit(id, &content);
                self.items.get_mut(id).expect("checked").mtime = mtime;
                Ok((200, id.clone()))
            }
        }
    }

    fn fragment(&mut self, sid: &str, range: &str, body: &[u8]) -> ResponseTemplate {
        let Some(session) = self.sessions.get_mut(sid) else { return error(404, "itemNotFound") };
        let start: u64 = range.strip_prefix("bytes ").and_then(|r| r.split('-').next()).and_then(|n| n.parse().ok()).unwrap_or(u64::MAX);
        if start != session.data.len() as u64 {
            return error(416, "invalidRange");
        }
        session.data.extend_from_slice(body);
        if (session.data.len() as u64) < session.size {
            return ResponseTemplate::new(202).set_body_json(json!({
                "nextExpectedRanges": [format!("{}-", session.data.len())],
                "expirationDateTime": "2099-01-01T00:00:00Z",
            }));
        }
        let session = self.sessions.remove(sid).expect("checked");
        match self.land(&session.target, session.data, session.mtime) {
            Ok((status, id)) => self.answer(status, &id),
            Err(answer) => answer,
        }
    }

    fn put_new(&mut self, parent: &str, name: &str) -> ResponseTemplate {
        match self.land(&Target::New { parent: parent.into(), name: name.into() }, Vec::new(), 0) {
            Ok((status, id)) => self.answer(status, &id),
            Err(answer) => answer,
        }
    }

    fn put_existing(&mut self, id: &str, if_match: Option<&str>) -> ResponseTemplate {
        let Some(item) = self.items.get(id) else { return error(404, "itemNotFound") };
        if !Self::guard(item, if_match) {
            return error(412, "preconditionFailed");
        }
        match self.land(&Target::Existing { id: id.into() }, Vec::new(), 0) {
            Ok((status, id)) => self.answer(status, &id),
            Err(answer) => answer,
        }
    }
}

struct Responder(Arc<Mutex<Cloud>>);

impl Respond for Responder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.0.lock().unwrap().handle(request)
    }
}

pub struct FakeGraph {
    pub server: MockServer,
    pub cloud: Arc<Mutex<Cloud>>,
}

impl FakeGraph {
    /// An empty drive: its root, `R`.
    pub async fn start() -> Self {
        let server = MockServer::start().await;
        let cloud = Arc::new(Mutex::new(Cloud { base: server.uri(), ..Cloud::default() }));
        cloud.lock().unwrap().add(FakeItem {
            id: ROOT.into(),
            parent: None,
            name: String::new(),
            folder: true,
            content: Vec::new(),
            hash: None,
            size: 0,
            etag: "e-R".into(),
            ctag: "c-R".into(),
            mtime: 0,
        });
        Mock::given(any()).respond_with(Responder(Arc::clone(&cloud))).mount(&server).await;
        Self { server, cloud }
    }

    /// A drive holding what the base holds, with the same tags and hashes.
    #[cfg(test)]
    pub async fn from_store(store: &Store) -> Self {
        let graph = Self::start().await;
        let rows = store.with(|s| s.all_items()).unwrap();
        graph.with(|cloud| {
            for row in rows {
                cloud.add(FakeItem {
                    parent: row.parent_id.clone(),
                    name: row.name.clone(),
                    folder: row.kind == Kind::Folder,
                    content: Vec::new(),
                    hash: row.quickxor.clone(),
                    size: row.size,
                    etag: row.etag.clone().unwrap_or_else(|| format!("e-{}", row.id)),
                    ctag: row.ctag.clone().unwrap_or_else(|| format!("c-{}", row.id)),
                    mtime: row.mtime,
                    id: row.id,
                });
            }
        });
        graph
    }

    pub fn with<T>(&self, f: impl FnOnce(&mut Cloud) -> T) -> T {
        f(&mut self.cloud.lock().unwrap())
    }

    pub fn client(&self) -> DriveClient {
        let base = Url::parse(&format!("{}/", self.server.uri())).unwrap();
        DriveClient::new(base, Arc::new(StaticToken::new("T")))
            .unwrap()
            .with_retry(RetryPolicy { attempts: 2, default_wait: Duration::from_millis(5), max_wait: Duration::from_millis(20) })
    }
}

/// Records what the worker tells its host.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Recorder {
    pub events: Mutex<Vec<ActivityRow>>,
    pub cycles: AtomicUsize,
    /// Cycles asked for with a Full reconcile.
    pub fulls: AtomicUsize,
    /// Why the write gate is closed; open while `None`.
    pub gate: Mutex<Option<String>>,
}

#[cfg(test)]
impl OutboxHost for Recorder {
    fn activity(&self, event: &ActivityRow) {
        self.events.lock().unwrap().push(event.clone());
    }

    fn cycle_wanted(&self) {
        self.cycles.fetch_add(1, Ordering::SeqCst);
    }

    fn full_cycle_wanted(&self) {
        self.fulls.fetch_add(1, Ordering::SeqCst);
    }

    fn may_write(&self) -> Result<(), String> {
        self.gate.lock().unwrap().clone().map_or(Ok(()), Err)
    }
}

#[cfg(test)]
impl Recorder {
    pub fn kinds(&self) -> Vec<String> {
        self.events.lock().unwrap().iter().map(|e| e.kind.clone()).collect()
    }
}

#[cfg(test)]
/// A worker for one folder against a fake OneDrive, driven by hand: each
/// [`engine`](Harness::engine) is a fresh start on the same store.
pub(crate) struct Harness {
    pub runtime: tokio::runtime::Runtime,
    pub graph: FakeGraph,
    pub host: Arc<Recorder>,
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
    root: SyncRoot,
    store: Store,
    locks: InodeLocks,
    pub limits: Limits,
    /// The helper and the fills a `move-out` row needs; `None` by default.
    pub moved_out: Mutex<Option<super::move_out::MoveOuts>>,
}

#[cfg(test)]
impl Harness {
    /// The fake OneDrive starts as the base is.
    pub fn new(root: &SyncRoot, store: &Store, locks: &InodeLocks) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let graph = runtime.block_on(FakeGraph::from_store(store));
        Self {
            runtime,
            graph,
            host: Arc::new(Recorder::default()),
            tree_lock: Arc::new(tokio::sync::Mutex::new(())),
            root: root.clone(),
            store: store.clone(),
            locks: locks.clone(),
            limits: Limits { small_slots: 4, large_slots: 2, small_max: 320 * 1024, chunk: 320 * 1024 },
            moved_out: Mutex::new(None),
        }
    }

    pub fn config(&self) -> WorkerConfig {
        WorkerConfig {
            root: self.root.clone(),
            store: self.store.clone(),
            drive: self.graph.client(),
            locks: self.locks.clone(),
            machine_name: "fedora".into(),
            tree_lock: Arc::clone(&self.tree_lock),
            host: self.host.clone(),
            limits: self.limits,
            moved_out: self.moved_out.lock().unwrap().clone(),
        }
    }

    /// A worker as a new daemon start would build it.
    pub fn engine(&self) -> Arc<Engine> {
        Arc::new(Engine::new(self.config()))
    }

    pub fn drain(&self, engine: &Arc<Engine>) {
        self.runtime.block_on(engine.drain(&CancellationToken::new()));
    }

    /// A fresh worker, run until nothing more can run.
    pub fn run(&self) -> Arc<Engine> {
        let engine = self.engine();
        self.drain(&engine);
        engine
    }
}
