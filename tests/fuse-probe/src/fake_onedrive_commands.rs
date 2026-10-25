//! The fake OneDrive and the commands the suite gives it, a line each.
//!
//! What it is made to hold: `file <id> <parent> <name> <path>`, `folder <id> <parent> <name>`,
//! `notebook <id> <parent> <name>` (a OneNote notebook), `edit <id> <path>`,
//! `rename <id> <parent> <name>`, `trash <id>` (the root's id is `R`).
//!
//! What it holds: `paths`, `id <path>`, `compare <path> <file>` (its content at a path against
//! a file's), `bin` (the ids in its recycle bin).
//!
//! What it was asked: `count <method> <fragment>`, `changes` (the requests that are not `GET`).
//!
//! How it answers: `cut <id> <bytes> <times>`, `delay <method> <fragment> <ms> <times>`,
//! `throttle <method> <fragment> <pass> <seconds> <times>`, `lose <method> <fragment> <times>`,
//! `free <bytes>|unlimited` (the space left), `outage on|off` (every request that is not a
//! `GET` answered `500`: a failure that passes, not a throttle, which `503` is to the daemon).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use konedrived::fake_onedrive::{FakeGraph, FakeItem};
use wiremock::{Mock, Request, ResponseTemplate};

/// The fake OneDrive, and whether it is in an outage. The outage is this type's own: an
/// answer put before the fake's, which the fake behind it knows nothing of.
pub struct FakeOneDriveCommands {
    graph: FakeGraph,
    is_in_outage: Arc<AtomicBool>,
}

impl FakeOneDriveCommands {
    /// An empty drive, listening on the loopback.
    pub async fn start() -> Self {
        let graph = FakeGraph::start().await;
        let is_in_outage = Arc::new(AtomicBool::new(false));
        let asked = Arc::clone(&is_in_outage);
        let refused = Mock::given(move |request: &Request| asked.load(Ordering::SeqCst) && request.method.as_str() != "GET");
        refused.respond_with(ResponseTemplate::new(500)).with_priority(1).mount(&graph.server).await;
        FakeOneDriveCommands { graph, is_in_outage }
    }

    pub fn graph(&self) -> &FakeGraph {
        &self.graph
    }

    /// What came of the command `words`; `None` when it is not one of the fake's.
    pub async fn answer(&mut self, words: &[&str]) -> Option<String> {
        let graph = &self.graph;
        let said = match words {
            ["file", id, parent, name, from] => {
                graph.with(|cloud| cloud.add_file(id, parent, name, &std::fs::read(from).unwrap()));
                String::new()
            }
            ["folder", id, parent, name] => {
                graph.with(|cloud| {
                    cloud.add(FakeItem {
                        id: id.to_string(),
                        parent: Some(parent.to_string()),
                        name: name.to_string(),
                        folder: true,
                        content: Vec::new(),
                        hash: None,
                        size: 0,
                        etag: format!("e-{id}"),
                        ctag: format!("c-{id}"),
                        mtime: 0,
                    })
                });
                String::new()
            }
            ["notebook", id, parent, name] => {
                graph.with(|cloud| cloud.add_notebook(id, parent, name));
                String::new()
            }
            ["edit", id, from] => {
                graph.with(|cloud| cloud.edit(id, &std::fs::read(from).unwrap()));
                String::new()
            }
            ["rename", id, parent, name] => {
                graph.with(|cloud| cloud.rename(id, parent, name));
                String::new()
            }
            ["trash", id] => {
                graph.with(|cloud| cloud.trash(id));
                String::new()
            }
            ["paths"] => graph.with(|cloud| cloud.paths()).join(" "),
            ["id", path] => graph.with(|cloud| cloud.at(path).map(|item| item.id.clone())).unwrap_or_else(|| "absent".into()),
            ["compare", path, file] => {
                let wanted = std::fs::read(file).unwrap();
                match graph.with(|cloud| cloud.at(path).map(|item| item.content.clone())) {
                    None => "absent".into(),
                    Some(held) if held == wanted => "equal".into(),
                    Some(held) => format!("differs: {} bytes, {:?}", held.len(), String::from_utf8_lossy(&held[..held.len().min(60)])),
                }
            }
            ["bin"] => graph.with(|cloud| cloud.bin.keys().cloned().collect::<Vec<_>>()).join(" "),
            ["count", method, fragment] => graph.with(|cloud| cloud.count(method, fragment)).to_string(),
            ["changes"] => graph.with(|cloud| cloud.log.iter().filter(|(method, _)| method != "GET").count()).to_string(),
            ["delay", method, fragment, ms, times] => {
                graph.with(|cloud| cloud.delay(method, fragment, Duration::from_millis(ms.parse().unwrap()), times.parse().unwrap()));
                String::new()
            }
            // The next `times` downloads of `id` end after `bytes` bytes.
            ["cut", id, bytes, times] => {
                graph.with(|cloud| {
                    let start = cloud.item(id).map(|item| item.content[..bytes.parse().unwrap()].to_vec()).unwrap_or_default();
                    cloud.script("GET", &format!("dl/{id}"), ResponseTemplate::new(200).set_body_bytes(start), times.parse().unwrap());
                });
                String::new()
            }
            ["throttle", method, fragment, pass, seconds, times] => {
                graph.with(|cloud| cloud.throttle(method, fragment, pass.parse().unwrap(), seconds.parse().unwrap(), times.parse().unwrap()));
                String::new()
            }
            ["lose", method, fragment, times] => {
                graph.with(|cloud| cloud.lose_answers(method, fragment, times.parse().unwrap()));
                String::new()
            }
            ["free", space] => {
                graph.with(|cloud| cloud.free = space.parse().ok());
                String::new()
            }
            ["outage", "on"] => {
                self.is_in_outage.store(true, Ordering::SeqCst);
                String::new()
            }
            ["outage", "off"] => {
                self.is_in_outage.store(false, Ordering::SeqCst);
                String::new()
            }
            _ => return None,
        };
        Some(said)
    }
}
