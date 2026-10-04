//! The listings of the enumeration in `names.rs`, brought into a read-only
//! folder. Nothing waits there and nothing is sent: after the listing the
//! folder is as OneDrive has it, and what was changed on this computer is
//! rescued out of it.

use std::collections::BTreeMap;

use super::names::{cloud_changes, place_in_onedrive, senseless, Case, CHAIN};
use super::*;
use crate::remote::testing::Options;

/// Every file below `dir`, hidden ones too.
fn files_below(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => files_below(&path, out),
            Ok(_) => out.push(path),
            Err(_) => {}
        }
    }
}

/// One combination in a read-only folder: what is wrong with how it ends.
/// `case.local`: 0 nothing was done here, 2 `docs/f.txt` is downloaded and
/// was changed here (by a user with their own chmod).
async fn run(case: Case) -> Vec<String> {
    let mut wrong = Vec::new();
    let w = Arc::new(World::new(Options { locked: true, ..Options::default() }).await);
    w.graph.with(|c| {
        c.add(folder_item("D", ROOT, "docs"));
        c.add_file("F", "D", "f.txt", b"one");
        c.add_file("T", ROOT, "top.txt", b"top");
        c.add_file("G", "D", "g.txt", b"g");
        c.add(folder_item("S", "D", "sub"));
        c.add_file("X", "S", "x.txt", b"x");
        c.add(folder_item("P", ROOT, "papers"));
        c.add_file("PF", "P", "p.txt", b"p");
        c.add(folder_item("L", ROOT, &"y".repeat(260)));
    });
    let listing = w.listed().await;
    let changed_here: &[u8] = b"changed here";
    if case.local == 2 {
        let at = w.path("docs/f.txt");
        write_version(&at, b"was", &w.cloud_ctag("F"));
        std::thread::sleep(std::time::Duration::from_millis(10));
        let file = placeholder::reopen_writable(&std::fs::File::open(&at).unwrap()).unwrap();
        file.set_len(0).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&file, changed_here, 0).unwrap();
    }
    w.graph.with(|c| cloud_changes(c, case));
    type InOneDrive = BTreeMap<String, (Option<String>, String, Vec<u8>)>;
    let in_onedrive = |w: &World| -> InOneDrive { w.graph.with(|c| c.items.values().map(|i| (i.id.clone(), (i.parent.clone(), i.name.clone(), i.content.clone()))).collect()) };
    let staged = in_onedrive(&w);
    if case.full {
        listing.request_full();
    }
    // It settles: the cycle of the listing, and then cycles that move nothing.
    let places = |w: &World| -> Vec<(String, String)> { objects(w).into_iter().map(|o| (o.0, o.1)).collect() };
    let mut last = None;
    let mut settled = false;
    for _ in 0..4 {
        if let Err(e) = listing.cycle(&tokio_util::sync::CancellationToken::new()).await {
            return vec![format!("a cycle failed: {e}")];
        }
        listing.join_replacements().await;
        let now = places(&w);
        settled = last.as_ref() == Some(&now);
        last = Some(now);
        if settled {
            break;
        }
    }
    let ends = last.expect("a cycle ran");
    if !settled {
        wrong.push(format!("does not settle in four cycles: {ends:?}"));
    }
    // And a Full reconcile moves nothing.
    listing.request_full();
    if let Err(e) = listing.cycle(&tokio_util::sync::CancellationToken::new()).await {
        wrong.push(format!("the Full cycle afterwards failed: {e}"));
    }
    listing.join_replacements().await;
    let on_disk = places(&w);
    if on_disk != ends {
        wrong.push(format!("a Full reconcile afterwards moved objects: {on_disk:?}"));
    }
    // Nothing is sent, and OneDrive is as the listing left it.
    if !w.sent().is_empty() {
        wrong.push(format!("sent to OneDrive: {:?}", w.sent()));
    }
    if in_onedrive(&w) != staged {
        wrong.push("OneDrive changed".into());
    }
    // The folder is as OneDrive has it: every item it can hold is here at
    // its place, every object here is its item's at that place, and nothing
    // is left in the holding directory.
    let held: Vec<(String, String)> = w.graph.with(|c| c.items.keys().filter(|item| item.as_str() != ROOT).filter_map(|item| Some((item.clone(), place_in_onedrive(c, item)?))).collect());
    for (item, path) in &held {
        if !on_disk.iter().any(|(at, carries)| at == path && carries == item) {
            wrong.push(format!("{item} is {path} in OneDrive and is not there here"));
        }
    }
    for (at, carries) in &on_disk {
        if !held.iter().any(|(item, path)| item == carries && path == at) {
            wrong.push(format!("{at} (carrying {carries:?}) is here and is not what OneDrive has there"));
        }
    }
    // Nothing waits, and the skipped list says of nothing that it is here.
    if !w.store.call(|s| s.deferred_ids()).await.unwrap().is_empty() {
        wrong.push("a change waits in a read-only folder".into());
    }
    for line in w.store.call(|s| s.skipped()).await.unwrap() {
        if line.here.is_some() || line.waits.is_some() {
            wrong.push(format!("the skipped list says an item is still here: {:?} {:?}", line.here, line.waits));
        }
    }
    // Nothing lost: what was changed here is in the folder or was rescued.
    if case.local == 2 {
        let mut files = Vec::new();
        files_below(w.root.path.parent().unwrap(), &mut files);
        files_below(&w.rescue_dir, &mut files);
        if !files.iter().any(|file| std::fs::read(file).is_ok_and(|data| data == changed_here)) {
            wrong.push("what docs/f.txt held is lost".into());
        }
    }
    wrong
}

/// The gate's listings in a read-only folder: every combination that makes
/// sense of the item OneDrive takes out of what the folder can hold, how,
/// and one more change in the same listing; with nothing done here, or
/// `docs/f.txt` changed here; in a Changed and in a Full reconcile. Each
/// must settle, send nothing, lose nothing, and end as OneDrive has it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_small_combination_in_a_read_only_folder_ends_as_onedrive_has_it() {
    let mut cases = Vec::new();
    for item in [None, Some(0), Some(1), Some(2), Some(3)] {
        for reason in if item.is_none() { vec!['N'] } else { vec!['N', 'M', 'R'] } {
            for other in 0..=CHAIN {
                for local in [0, 2] {
                    for full in [false, true] {
                        let case = Case { item, reason, other, local, full };
                        if !senseless(case) {
                            cases.push(case);
                        }
                    }
                }
            }
        }
    }
    let count = cases.len();
    let mut runs = tokio::task::JoinSet::new();
    let mut wrong = Vec::new();
    let mut cases = cases.into_iter();
    loop {
        while runs.len() < 12 {
            let Some(case) = cases.next() else { break };
            runs.spawn(async move { (case, run(case).await) });
        }
        match runs.join_next().await {
            None => break,
            Some(Ok((_, what))) if what.is_empty() => {}
            Some(Ok((case, what))) => wrong.push(format!("{case:?}:\n    {}", what.join("\n    "))),
            Some(Err(panic)) => wrong.push(format!("a combination panicked: {panic}")),
        }
    }
    wrong.sort();
    println!("{count} combinations");
    assert!(wrong.is_empty(), "{} of {count} combinations break an invariant:\n{}", wrong.len(), wrong.join("\n"));
}
