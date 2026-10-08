//! The daemon's own service for one account, with a folder served through FUSE, over the fake
//! OneDrive: `fuse-service <state directory>`. It prints `ready` and then takes one command a
//! line on its input, answering each with `done <line>: <what came of it>`.
//!
//! The cloud's: `file <id> <parent> <name> <path>`, `folder <id> <parent> <name>`,
//! `edit <id> <path>`, `rename <id> <parent> <name>`, `trash <id>` (the root's id is `R`).
//! The folder's: `register <path>`, `forget`, `refresh`, `hydrate <path>`, `dehydrate <path>`,
//! `free-up <path>`, `free-up-space`, `pin <path>`, `unpin <path>`, `state <path>`,
//! `menu <path>`, `status`, `activity`.

use std::path::{Path, PathBuf};

use konedrived::account::state::{AccountSnapshot, SignInState, StateHandle};
use konedrived::fake_onedrive::{FakeGraph, FakeItem};
use konedrived::sync::testing;
use konedrived::sync::wiring::SyncPaths;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::main]
async fn main() {
    let state = PathBuf::from(std::env::args().nth(1).expect("a state directory"));
    let graph = FakeGraph::start().await;
    let account = StateHandle::new(AccountSnapshot { state: SignInState::SignedIn, ..AccountSnapshot::default() });
    let paths = SyncPaths { tree_db: state.join("fuse.sqlite"), rescue_dir: state.join("rescued"), thumbnails: None };
    // The folder is recorded in a `config.toml` that stays: a second run finds it there, as a
    // daemon started again does, and brings it up.
    let persist = testing::persist(&state.join("config.toml"));
    let service = testing::wiring().account(account).persist(persist).onedrive(graph.client(), paths).fuse().build();
    service.resume().await;
    println!("ready");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let words: Vec<&str> = line.split_whitespace().collect();
        let said = |r: Result<String, String>| r.unwrap_or_else(|e| format!("refused: {e}"));
        let path = |p: &&str| Path::new(*p).to_owned();
        let answer = match words.as_slice() {
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
            ["delay", method, fragment, ms, times] => {
                graph.with(|cloud| cloud.delay(method, fragment, std::time::Duration::from_millis(ms.parse().unwrap()), times.parse().unwrap()));
                String::new()
            }
            ["register", p] => said(service.register_root(&path(p)).await.map(|()| "registered".into()).map_err(|e| e.to_string())),
            ["forget"] => said(service.unregister_root().await.map(|()| "forgotten".into()).map_err(|e| e.to_string())),
            ["refresh"] => said(service.refresh().await.map(|()| "refreshed".into()).map_err(|e| e.to_string())),
            ["hydrate", p] => said(service.hydrate_now(&path(p)).await.map(|()| "downloaded".into()).map_err(|e| e.to_string())),
            ["dehydrate", p] => said(service.dehydrate(&path(p)).await.map(|()| "freed".into()).map_err(|e| e.to_string())),
            ["free-up", p] => said(service.free_up(&[path(p)]).await.map(|f| format!("{f:?}")).map_err(|e| e.to_string())),
            ["free-up-space"] => said(service.free_up_space().await.map(|f| format!("{f:?}")).map_err(|e| e.to_string())),
            ["pin", p] => said(service.pin(&[path(p)]).await.map(|n| format!("{n} pinned")).map_err(|e| e.to_string())),
            ["unpin", p] => said(service.unpin(&[path(p)]).await.map(|n| format!("{n} unpinned")).map_err(|e| e.to_string())),
            ["state", p] => service.item_state(&path(p)).await,
            ["menu", p] => {
                let part = service.menu_part(&[path(p)]).await;
                format!("taken {:?}, kept by {:?}, free-up refused {:?}", part.taken, part.kept_by, part.free_up_refused)
            }
            ["status"] => {
                let s = service.state().get();
                format!(
                    "state {:?}, listed {}, placed {}, pinned {}, local bytes {}, last checked {}, last error {:?}, the helper is not needed {}",
                    s.folder.root_state, s.cycle.items_listed, s.cycle.items_placed, s.local.pinned_count, s.local.local_bytes, s.cycle.last_checked > 0, s.folder.last_error, s.folder.helper_not_needed
                )
            }
            ["activity"] => {
                let recent = service.recent_activity(20).await;
                recent.iter().map(|e| format!("{:?}", e)).collect::<Vec<_>>().join(" | ")
            }
            _ => "not a command".into(),
        };
        println!("done {line}: {answer}");
    }
}
