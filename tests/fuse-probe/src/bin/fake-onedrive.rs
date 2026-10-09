//! The fake OneDrive as a process of its own, which a daemon that is killed leaves standing:
//! `fake-onedrive`. It prints `ready <its address>` and then takes one command a line on its
//! input (`fuse_probe::fake_onedrive_commands`), answering each with
//! `done <line>: <what came of it>`.

use fuse_probe::fake_onedrive_commands::FakeOneDriveCommands;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::main]
async fn main() {
    let mut onedrive = FakeOneDriveCommands::start().await;
    println!("ready {}", onedrive.graph().server.uri());
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let words: Vec<&str> = line.split_whitespace().collect();
        let answer = onedrive.answer(&words).await.unwrap_or_else(|| "not a command".into());
        println!("done {line}: {answer}");
    }
}
