//! The daemon's fake OneDrive as a process. It prints `url <Graph's root>` and then takes one
//! command a line on its input:
//! `file <id> <parent> <name> <path>` (content from a local file), `folder <id> <parent> <name>`,
//! `edit <id> <path>`, `rename <id> <parent> <name>`, `trash <id>`, `delay <method> <path fragment> <ms> <times>`. The root's id is `R`.

use konedrived::fake_onedrive::{FakeGraph, FakeItem};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::main]
async fn main() {
    let graph = FakeGraph::start().await;
    println!("url {}/", graph.server.uri());
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let words: Vec<&str> = line.split_whitespace().collect();
        graph.with(|cloud| match words.as_slice() {
            ["file", id, parent, name, path] => cloud.add_file(id, parent, name, &std::fs::read(path).unwrap()),
            ["folder", id, parent, name] => cloud.add(FakeItem {
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
            }),
            ["edit", id, path] => cloud.edit(id, &std::fs::read(path).unwrap()),
            ["rename", id, parent, name] => cloud.rename(id, parent, name),
            ["trash", id] => cloud.trash(id),
            ["delay", method, fragment, ms, times] => {
                cloud.delay(method, fragment, std::time::Duration::from_millis(ms.parse().unwrap()), times.parse().unwrap())
            }
            _ => eprintln!("fake-cloud: not a command: {line}"),
        });
        println!("done {line}");
    }
}
