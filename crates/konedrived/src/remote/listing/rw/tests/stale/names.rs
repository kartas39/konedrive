//! Names taken and given in one listing while something can no longer be
//! placed: three shapes a review ran, and one bounded enumeration of the
//! rest. Every one asserts the same four things: the folder settles (after
//! the listing is applied, cycles with nothing new move no object and send
//! nothing), nothing is lost on disk, nothing is sent to OneDrive that the
//! user did not do, and what the skipped list says is true.

use super::*;

/// What the user's own pending change of content may send, and nothing
/// else may be sent at all.
fn is_an_upload(request: &(String, String)) -> bool {
    (request.0 == "POST" && request.1.ends_with("/createUploadSession")) || (request.0 == "PUT" && (request.1.contains("upload/") || request.1.ends_with("/content")))
}

/// A file downloaded and changed here, with its row recorded.
async fn changed_here(w: &World, rel: &str, id: &str) {
    write_version(&w.path(rel), b"was", &w.cloud_ctag(id));
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(w.path(rel), b"changed here").unwrap();
    let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    let mut batch = crate::local::Batch::new();
    batch.written(Path::new(dir), std::ffi::OsStr::new(name), None);
    w.examine(batch).await;
}

/// Cycles, each with the watcher's and the worker's part, until two in a
/// row leave every object and the request count as they were, at most eight;
/// then the four invariants. What is wrong, if anything.
async fn invariants(w: &World, listing: &Arc<Listing>, full: bool, kept: &[(&str, &[u8])], known: &[(&str, &str)]) -> Result<(), String> {
    if full {
        listing.request_full();
    }
    let mut last = None;
    let mut settled = false;
    for _ in 0..8 {
        if let Err(e) = listing.cycle(&tokio_util::sync::CancellationToken::new()).await {
            return Err(format!("a cycle failed: {e}"));
        }
        listing.join_replacements().await;
        w.examine_handed().await;
        w.upload().await;
        let now = (objects(w), w.sent().len());
        settled = last.as_ref() == Some(&now);
        last = Some(now);
        if settled {
            break;
        }
    }
    if !settled {
        return Err(format!("does not settle in eight cycles: {:?}", last.map(|(objects, _)| objects.into_iter().map(|o| format!("{} {}", o.0, o.1)).collect::<Vec<_>>())));
    }
    let not_the_users: Vec<_> = w.sent().into_iter().filter(|request| !is_an_upload(request) && !known.contains(&(request.0.as_str(), request.1.as_str()))).collect();
    if !not_the_users.is_empty() {
        return Err(format!("sent to OneDrive: {not_the_users:?}"));
    }
    if !w.graph.with(|c| c.bin.is_empty()) {
        return Err("something went to OneDrive's recycle bin".into());
    }
    // Nothing lost: each piece of local work is in a file here, or in OneDrive.
    let here: Vec<Vec<u8>> = objects(w).iter().filter_map(|o| std::fs::read(w.path(&o.0)).ok()).collect();
    for (what, content) in kept {
        let there = w.graph.with(|c| c.items.values().any(|i| i.content == *content));
        if !here.iter().any(|file| file == content) && !there {
            return Err(format!("{what} is lost"));
        }
    }
    // The list is true: every path a line names exists, and what waits is on it.
    let lines = w.store.call(|s| s.skipped()).await.map_err(|e| e.to_string())?;
    let mut still_here = Vec::new();
    for line in &lines {
        for named in line.here.iter().cloned().chain(line.waits.as_ref().and_then(WaitsFor::path).map(PathBuf::from)) {
            if std::fs::symlink_metadata(w.path(&named.display().to_string())).is_err() {
                return Err(format!("the skipped list names {} where nothing is ({:?})", named.display(), line.waits));
            }
        }
        still_here.extend(line.here.clone());
    }
    for id in w.store.call(|s| s.deferred_ids()).await.map_err(|e| e.to_string())? {
        let at = w.store.call({ let id = id.clone(); move |s| s.locate(konedrive_tree::Table::Items, &id) }).await.map_err(|e| e.to_string())?.map(|at| at.rel);
        if !at.as_ref().is_some_and(|at| still_here.iter().any(|line| at.starts_with(line))) {
            return Err(format!("{id} waits (at {at:?}) and no line of the skipped list says so"));
        }
    }
    Ok(())
}

/// A child OneDrive moved out of a folder that leaves, to a name a local
/// change still holds, while a newcomer takes the folder's name: the child
/// goes back into its own folder, where that stands now, never into the
/// newcomer; nothing is sent for it, and it moves once the name is free.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_that_cannot_be_placed_yet_goes_back_into_its_own_folder_not_the_newcomers() {
    for full in [false, true] {
        let w = Arc::new(World::read_write().await);
        let listing = w.listed().await;
        write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
        changed_here(&w, "top.txt", "T").await;
        w.graph.with(|c| {
            c.rename("D", ROOT, &long_name());
            c.add(folder_item("Y", ROOT, "docs"));
            c.rename("T", ROOT, "other.txt");
            c.rename("F", ROOT, "top.txt");
        });
        invariants(&w, &listing, full, &[("the change of top.txt", b"changed here")], &[]).await.unwrap_or_else(|wrong| panic!("full={full}: {wrong}"));
        w.graph.with(|c| {
            let f = c.item("F").unwrap();
            assert_eq!((f.parent.as_deref(), f.name.as_str()), (Some(ROOT), "top.txt"), "full={full}: as OneDrive had it");
            assert_eq!(c.item("T").unwrap().content, b"changed here", "full={full}");
        });
        assert_eq!((id_at(&w.path("top.txt")).as_deref(), id_at(&w.path("other.txt")).as_deref(), id_at(&w.path("docs")).as_deref()), (Some("F"), Some("T"), Some("Y")), "full={full}");
    }
}

/// A folder that waits steps aside below a folder OneDrive renamed in the
/// same listing, and so does a file with a change waiting: the rows follow
/// to where the objects stand, and the copy name is never sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_step_aside_below_a_folder_renamed_in_the_same_listing_sends_no_name() {
    for (full, file) in [(false, false), (true, false), (false, true), (true, true)] {
        let case = format!("full={full} file={file}");
        let w = Arc::new(World::read_write().await);
        let listing = w.listed().await;
        w.graph.with(|c| {
            c.add(folder_item("S", "D", "sub"));
            c.add_file("SF", "S", "x.txt", b"x");
        });
        w.cycle(&listing).await;
        if file {
            changed_here(&w, "docs/f.txt", "F").await;
        } else {
            w.blocked_file_in("docs/sub").await;
        }
        w.graph.with(|c| {
            c.rename("D", ROOT, "papers");
            if file {
                c.rename("F", "D", &long_name());
                c.add_file("Z", "D", "f.txt", b"another");
            } else {
                c.rename("S", "D", &long_name());
                c.add(folder_item("N", "D", "sub"));
            }
        });
        let kept: &[(&str, &[u8])] = if file { &[("the change of f.txt", b"changed here")] } else { &[("the new file", b"new")] };
        invariants(&w, &listing, full, kept, &[]).await.unwrap_or_else(|wrong| panic!("{case}: {wrong}"));
        if file {
            assert_eq!(w.graph.with(|c| c.item("F").map(|f| f.content.clone())), Some(b"changed here".to_vec()), "{case}");
            assert_eq!(id_at(&w.path("papers/f.txt")).as_deref(), Some("Z"), "{case}");
        } else {
            assert_eq!((id_at(&w.path("papers/sub")).as_deref(), id_at(&w.path("papers/sub-fedora")).as_deref()), (Some("N"), Some("S")), "{case}");
            let rows: Vec<PathBuf> = w.store.call(|s| s.outbox_rows()).await.unwrap().into_iter().map(|row| row.rel).collect();
            assert_eq!(rows, [PathBuf::from("papers/sub-fedora/n:ew.txt")], "{case}: the row is where its file is");
        }
    }
}

/// One item of a small folder becomes unplaceable (or none), together with
/// one more change in the same listing and one piece of local work, in a
/// Changed and in a Full reconcile: every combination that makes sense.
/// The folder is `docs` with `f.txt`, `g.txt` and `sub/x.txt`, and `top.txt`.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn every_small_combination_of_an_unplaceable_item_another_change_and_local_work_keeps_the_invariants() {
    // The item, its folder in OneDrive, its name, the folder as a path here.
    const ITEMS: [(&str, &str, &str, &str); 4] = [("D", ROOT, "docs", ""), ("S", "D", "sub", "docs"), ("F", "D", "f.txt", "docs"), ("T", ROOT, "top.txt", "")];
    let mut runs = tokio::task::JoinSet::new();
    let mut count = 0;
    for unplaceable in [None, Some(0), Some(1), Some(2), Some(3)] {
        for other in 0..7 {
            for local in 0..3 {
                for full in [false, true] {
                    let item = unplaceable.map(|n| ITEMS[n]);
                    let id = item.map_or("", |i| i.0);
                    // What cannot be: a name freed by nothing, a child of a file, a folder above the root.
                    let senseless = match other {
                        1..=3 => item.is_none(),
                        4 => !matches!(id, "D" | "S"),
                        5 => matches!(id, "D" | "T"),
                        _ => false,
                    };
                    if senseless {
                        continue;
                    }
                    count += 1;
                    let case = format!("unplaceable={id:?} other={other} local={local} full={full}");
                    runs.spawn(async move {
                        let w = Arc::new(World::read_write().await);
                        let listing = w.listed().await;
                        w.graph.with(|c| {
                            c.add_file("G", "D", "g.txt", b"g");
                            c.add(folder_item("S", "D", "sub"));
                            c.add_file("X", "S", "x.txt", b"x");
                        });
                        w.cycle(&listing).await;
                        let mut kept: Vec<(&str, &[u8])> = Vec::new();
                        match local {
                            1 => {
                                w.blocked_file_in(if id == "S" { "docs/sub" } else { "docs" }).await;
                                kept.push(("the new file", b"new"));
                            }
                            2 => {
                                changed_here(&w, "docs/f.txt", "F").await;
                                kept.push(("the change of f.txt", b"changed here"));
                            }
                            _ => {}
                        }
                        w.graph.with(|c| {
                            if let Some((id, parent, _, _)) = item {
                                c.rename(id, parent, &long_name());
                            }
                            let (parent, name) = item.map_or((ROOT, ""), |i| (i.1, i.2));
                            match other {
                                1 => c.add_file("Z", parent, name, b"another"),
                                2 => {
                                    c.add(folder_item("Y", parent, name));
                                    c.add_file("YC", "Y", "there.txt", b"there");
                                }
                                // Another item that was there takes the name.
                                3 => c.rename(match id { "D" => "T", "S" => "G", "F" => "X", _ => "F" }, parent, name),
                                4 => c.rename(if id == "D" { "G" } else { "X" }, ROOT, "moved-out.txt"),
                                5 => c.rename("D", ROOT, "papers"),
                                // Two others exchange their places.
                                6 if id == "F" => {
                                    c.rename("T", "D", "g.txt-swap");
                                    c.rename("G", ROOT, "top.txt");
                                    c.rename("T", "D", "g.txt");
                                }
                                6 => {
                                    c.rename("F", "D", "f.txt-swap");
                                    c.rename("G", "D", "f.txt");
                                    c.rename("F", "D", "g.txt");
                                }
                                _ => {}
                            }
                        });
                        // The one request known to go out that nobody made
                        // (limitations log F259, on `dev` before this too):
                        // the changed file's name was exchanged with
                        // another's in OneDrive, the worker cannot give it
                        // OneDrive's name here while the other file has it,
                        // and sends its own name back, which OneDrive
                        // refuses; both versions are then kept.
                        let known: &[(&str, &str)] = if other == 6 && local == 2 { &[("PATCH", "me/drive/items/F")] } else { &[] };
                        let wrong = invariants(&w, &listing, full, &kept, known).await.err();
                        let once = w.sent().iter().filter(|request| request.0 == "PATCH").count() <= known.len();
                        wrong.or((!once).then(|| "the known request went out more than once".to_owned())).map(|wrong| format!("{case}: {wrong}"))
                    });
                }
            }
        }
    }
    let mut wrong = Vec::new();
    while let Some(run) = runs.join_next().await {
        match run {
            Ok(None) => {}
            Ok(Some(what)) => wrong.push(what),
            Err(panic) => wrong.push(format!("a combination panicked: {panic}")),
        }
    }
    wrong.sort();
    assert!(wrong.is_empty(), "{} of {count} combinations break an invariant:\n{}", wrong.len(), wrong.join("\n"));
}

/// A child OneDrive moved out of a folder that leaves, to a name a file
/// made here holds for as long as a program has it open: nothing is moved,
/// not even to the holding directory and back; the folder stays for its
/// child, and its line names the child where it is. Once the name is free
/// the child is moved and the folder leaves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_whose_new_name_is_held_here_is_not_touched_until_the_name_is_free() {
    for full in [false, true] {
        let w = Arc::new(World::read_write().await);
        let listing = w.listed().await;
        let child = handle_of(&w.path("docs/f.txt"));
        std::fs::write(w.path("held.txt"), b"mine").unwrap();
        let open = std::fs::OpenOptions::new().append(true).open(w.path("held.txt")).unwrap();
        let mut batch = crate::local::Batch::new();
        batch.name(Path::new(""), std::ffi::OsStr::new("held.txt"));
        w.examine(batch).await;
        w.graph.with(|c| {
            c.rename("D", ROOT, &long_name());
            c.rename("F", ROOT, "held.txt");
        });
        if full {
            listing.request_full();
        }
        let mut seen = Vec::new();
        for _ in 0..3 {
            w.cycle(&listing).await;
            w.examine_handed().await;
            seen.push(objects(&w));
        }
        assert_eq!(seen[0], seen[1], "full={full}: nothing is moved while the name is held");
        assert_eq!(seen[1], seen[2], "full={full}");
        assert_eq!(handle_of(&w.path("docs/f.txt")), child, "full={full}");
        assert_eq!(w.still_here().await, [("docs".to_owned(), WaitsFor::MovedAway("docs/f.txt".into()))], "full={full}");
        assert_eq!(w.sent(), [], "full={full}");
        drop(open);
        w.scan_and_upload().await;
        w.rounds(&listing, 3).await;
        assert_eq!(handle_of(&w.path("held.txt")), child, "full={full}: moved once the name is free, the same object");
        assert!(!w.path("docs").exists(), "full={full}: and its folder left");
        assert!(w.graph.with(|c| c.items.values().any(|i| i.content == b"mine")), "full={full}: {:?}", w.graph.with(|c| c.paths()));
        assert_eq!((w.deletes(), w.graph.with(|c| c.count("PATCH", "items/"))), (0, 0), "full={full}");
    }
}
