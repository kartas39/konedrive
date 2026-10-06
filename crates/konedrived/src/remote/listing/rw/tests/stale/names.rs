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
            // A file made in a folder records no move of the folder.
            let rows: Vec<_> = w.store.call(|s| s.outbox_rows()).await.unwrap().into_iter().map(|row| (row.kind, row.rel)).collect();
            assert_eq!(rows, [(konedrive_tree::outbox::OutboxKind::Create, PathBuf::from("docs/sub/n:ew.txt"))], "{case}");
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

/// One combination of the enumeration.
#[derive(Clone, Copy, Debug)]
pub(super) struct Case {
    /// Which item OneDrive takes out of what the folder can hold, if any.
    pub(super) item: Option<usize>,
    /// How: a name too long (`N`), or a move into a folder that is not
    /// placed (`M`). In a chain also `R`: a rename the folder can hold.
    pub(super) reason: char,
    /// What else the same listing does (see [`cloud_changes`]).
    pub(super) other: u8,
    /// What was done on this computer before (see [`local_work`]).
    pub(super) local: u8,
    /// Which file, downloaded here and unchanged, gets a new version in the
    /// same listing: 0 none, 1 `docs/sub/x.txt`, 2 `docs/g.txt`.
    pub(super) version: u8,
    pub(super) full: bool,
}

/// The files a combination may give a new version: item, path here, content.
pub(super) const VERSIONED: [(&str, &str, &[u8]); 2] = [("X", "docs/sub/x.txt", b"x"), ("G", "docs/g.txt", b"g")];

/// The file of [`Case::version`], downloaded here as OneDrive has it.
pub(super) fn download_versioned(w: &World, case: Case) {
    if let Some((id, rel, content)) = case.version.checked_sub(1).map(|n| VERSIONED[usize::from(n)]) {
        write_version(&w.path(rel), content, &w.cloud_ctag(id));
    }
}

/// Every file here whose version is not the base's while nothing waits for
/// it: the base is ahead of the disk, and a change made here now would go
/// up against a version this computer never had.
pub(super) async fn base_ahead_of_disk(w: &World) -> Vec<String> {
    let deferred = w.store.call(|s| s.deferred_ids()).await.unwrap();
    let mut ahead = Vec::new();
    for (at, id, _, _) in objects(w) {
        let path = w.path(&at);
        if id.is_empty() || deferred.contains(&id) || !path.is_file() {
            continue;
        }
        let on_disk = std::fs::File::open(&path).ok().and_then(|file| placeholder::read_ctag(&file).ok().flatten());
        let base = w.base(&id).and_then(|row| row.ctag);
        if base.is_some() && on_disk != base {
            ahead.push(format!("{at} holds version {on_disk:?}, the base has {base:?}, and nothing waits"));
        }
    }
    ahead
}

/// The item, its folder in OneDrive, its name.
const ITEMS: [(&str, &str, &str); 4] = [("D", ROOT, "docs"), ("S", "D", "sub"), ("F", "D", "f.txt"), ("T", ROOT, "top.txt")];
/// A chain: the item leaves (or is renamed), `g.txt` takes its name, `top.txt` takes `g.txt`'s.
pub(super) const CHAIN: u8 = 8;
/// The folder `docs/sub` is moved to the root, with what is in it.
pub(super) const SUB_OUT: u8 = 9;
/// Two folders, `docs` and `papers`, exchange names.
const FOLDERS: u8 = 7;
/// Two files exchange names (or places).
const FILES: u8 = 6;

/// What OneDrive does in one listing.
pub(super) fn cloud_changes(c: &mut crate::fake_onedrive::Cloud, case: Case) {
    let item = case.item.map(|n| ITEMS[n]);
    let id = item.map_or("", |i| i.0);
    if let Some((id, parent, name)) = item {
        match case.reason {
            'M' => c.rename(id, "L", name),
            'R' => c.rename(id, parent, "z.txt"),
            _ => c.rename(id, parent, &long_name()),
        }
    }
    let (parent, name) = item.map_or((ROOT, ""), |i| (i.1, i.2));
    match case.other {
        1 => c.add_file("Z", parent, name, b"another"),
        2 => {
            c.add(folder_item("Y", parent, name));
            c.add_file("YC", "Y", "there.txt", b"there");
        }
        // Another item that was there takes the name.
        3 => c.rename(match id { "D" => "T", "S" => "G", "F" => "X", _ => "F" }, parent, name),
        // Something is moved out of a folder to the root.
        4 => c.rename(if id == "D" { "G" } else { "X" }, ROOT, "moved-out.txt"),
        5 => c.rename("D", ROOT, "papers-2"),
        FILES if id == "F" => {
            c.rename("T", "D", "g.txt-swap");
            c.rename("G", ROOT, "top.txt");
            c.rename("T", "D", "g.txt");
        }
        FILES => {
            c.rename("F", "D", "f.txt-swap");
            c.rename("G", "D", "f.txt");
            c.rename("F", "D", "g.txt");
        }
        FOLDERS => {
            c.rename("D", ROOT, "docs-swap");
            c.rename("P", ROOT, "docs");
            c.rename("D", ROOT, "papers");
        }
        CHAIN => {
            c.rename("G", "D", "f.txt");
            c.rename("T", "D", "g.txt");
        }
        SUB_OUT => c.rename("S", ROOT, "sub"),
        _ => {}
    }
    match case.version {
        1 => c.edit("X", b"x, a newer version"),
        2 => c.edit("G", b"g, a newer version"),
        _ => {}
    }
}

/// Whether the combination cannot be: a name freed by nothing, a folder
/// above the root, a folder both gone and exchanged, a chain with no head.
pub(super) fn senseless(case: Case) -> bool {
    let id = case.item.map_or("", |n| ITEMS[n].0);
    // A rename the folder can hold is the head of a chain only.
    if (case.reason == 'R') != (case.other == CHAIN && case.reason != 'N' && case.reason != 'M') {
        return true;
    }
    match case.other {
        1..=3 => case.item.is_none(),
        5 | FOLDERS => id == "D",
        CHAIN => id != "F",
        SUB_OUT => id == "S",
        _ => false,
    }
}

/// What the user did before the listing came; the requests that are the
/// user's own (each at most once), and the item a delete or a rename made
/// here is of.
async fn local_work(w: &World, case: Case) -> (Vec<(String, String)>, &'static str) {
    let id = case.item.map_or("", |n| ITEMS[n].0);
    let (target_rel, target) = if id == "S" { ("docs/sub/x.txt", "X") } else { ("docs/g.txt", "G") };
    let (dir, name) = target_rel.rsplit_once('/').unwrap();
    let examine_names = |dir: &str, names: &[&str]| {
        let mut batch = crate::local::Batch::new();
        for name in names {
            batch.name(Path::new(dir), std::ffi::OsStr::new(name));
        }
        batch
    };
    let mut users = Vec::new();
    let mut of = "";
    match case.local {
        1 => w.blocked_file_in(if id == "S" { "docs/sub" } else { "docs" }).await,
        2 => changed_here(w, "docs/f.txt", "F").await,
        // A rename made here.
        3 => {
            std::fs::rename(w.path(target_rel), w.path(&format!("{dir}/h.txt"))).unwrap();
            w.examine(examine_names(dir, &[name, "h.txt"])).await;
            users.push(("PATCH".to_owned(), format!("me/drive/items/{target}")));
            of = target;
        }
        // A change of content and a rename, both made here.
        4 => {
            changed_here(w, "docs/f.txt", "F").await;
            std::fs::rename(w.path("docs/f.txt"), w.path("docs/h.txt")).unwrap();
            w.examine(examine_names("docs", &["f.txt", "h.txt"])).await;
            users.push(("PATCH".to_owned(), "me/drive/items/F".to_owned()));
            of = "F";
        }
        // A delete made here.
        5 => {
            std::fs::remove_file(w.path(target_rel)).unwrap();
            w.examine(examine_names(dir, &[name])).await;
            users.push(("DELETE".to_owned(), format!("me/drive/items/{target}")));
            of = target;
        }
        _ => {}
    }
    if case.other == FOLDERS {
        std::fs::write(w.path("papers/b:1.txt"), b"blocked").unwrap();
        w.examine(examine_names("papers", &["b:1.txt"])).await;
    }
    (users, of)
}

/// Where OneDrive has item `id`, as a path here; `None` where the folder
/// cannot hold it.
pub(super) fn place_in_onedrive(c: &crate::fake_onedrive::Cloud, id: &str) -> Option<String> {
    let mut names = Vec::new();
    let mut at = id.to_owned();
    while at != ROOT {
        let item = c.items.get(&at)?;
        if item.name.len() > 255 {
            return None;
        }
        names.push(item.name.clone());
        at = item.parent.clone()?;
    }
    names.reverse();
    Some(names.join("/"))
}

/// One combination, run: what is wrong with how it ends, each thing once.
async fn run(case: Case) -> Vec<String> {
    use std::collections::BTreeMap;
    let mut wrong = Vec::new();
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    w.graph.with(|c| {
        c.add_file("G", "D", "g.txt", b"g");
        c.add(folder_item("S", "D", "sub"));
        c.add_file("X", "S", "x.txt", b"x");
        c.add(folder_item("P", ROOT, "papers"));
        c.add_file("PF", "P", "p.txt", b"p");
        c.add(folder_item("L", ROOT, &"y".repeat(260)));
    });
    w.cycle(&listing).await;
    download_versioned(&w, case);
    let (users, of) = local_work(&w, case).await;
    let id = case.item.map_or("", |n| ITEMS[n].0);
    // Every file with data here before: none of it may be lost.
    let data_before: Vec<(String, Vec<u8>)> = objects(&w).iter().filter_map(|o| Some((o.0.clone(), std::fs::read(w.path(&o.0)).ok()?))).filter(|(at, data)| !data.is_empty() && crate::remote::testing::state_at(&w.path(at)) != Some(State::OnlineOnly)).filter(|(at, _)| case.version == 0 || VERSIONED[usize::from(case.version - 1)].1 != at).collect();
    w.graph.with(|c| {
        cloud_changes(c, case);
        // A new version takes a moment to come: what a cycle commits is
        // looked at before the replacement has landed.
        if case.version != 0 {
            c.delay("GET", "dl/", std::time::Duration::from_millis(100), 4);
        }
    });
    type InOneDrive = BTreeMap<String, (Option<String>, String, Vec<u8>)>;
    let in_onedrive = |w: &World| -> InOneDrive { w.graph.with(|c| c.items.values().map(|i| (i.id.clone(), (i.parent.clone(), i.name.clone(), i.content.clone()))).collect()) };
    let staged = in_onedrive(&w);
    if case.full {
        listing.request_full();
    }
    // I-c: it settles.
    let mut last = None;
    let mut settled = false;
    for _ in 0..8 {
        if let Err(e) = listing.cycle(&tokio_util::sync::CancellationToken::new()).await {
            return vec![format!("a cycle failed: {e}")];
        }
        // What the cycle committed, before the new version has landed.
        for ahead in base_ahead_of_disk(&w).await {
            if !wrong.contains(&ahead) {
                wrong.push(ahead);
            }
        }
        listing.join_replacements().await;
        w.examine_handed().await;
        w.upload().await;
        let now = (objects(&w), w.sent().len());
        settled = last.as_ref() == Some(&now);
        last = Some(now);
        if settled {
            break;
        }
    }
    let (ends, sent_then) = last.expect("a cycle ran");
    if !settled {
        wrong.push(format!("does not settle in eight cycles: {:?}", ends.iter().map(|o| format!("{}={}", o.0, o.1)).collect::<Vec<_>>()));
    }
    // And a scan of the whole folder with one more round moves nothing and sends nothing.
    w.scan_and_upload().await;
    if let Err(e) = listing.cycle(&tokio_util::sync::CancellationToken::new()).await {
        wrong.push(format!("the cycle after a scan of the whole folder failed: {e}"));
    }
    listing.join_replacements().await;
    w.examine_handed().await;
    w.upload().await;
    let on_disk: Vec<(String, String)> = objects(&w).into_iter().map(|o| (o.0, o.1)).collect();
    if on_disk != ends.iter().map(|o| (o.0.clone(), o.1.clone())).collect::<Vec<_>>() {
        wrong.push(format!("a scan of the whole folder and one more round moved objects: {on_disk:?}"));
    }
    wrong.extend(base_ahead_of_disk(&w).await);
    let sent = w.sent();
    if sent.len() != sent_then {
        wrong.push(format!("a scan of the whole folder and one more round sent {:?}", &sent[sent_then..]));
    }
    // I-d: only what the user did is sent. An upload only where content
    // waits, of that file; the user's rename or delete once; and the one
    // request nobody made (the name sent back), once, where `f.txt` has content
    // waiting and OneDrive exchanged its name with `g.txt`'s.
    let content = matches!(case.local, 2 | 4);
    let f259 = case.other == FILES && case.local == 2 && id != "F";
    let (mut known, mut own, mut sessions) = (0, 0, 0);
    for request in &sent {
        if is_an_upload(request) {
            let of_the_file = request.0 == "PUT" || request.1.contains("items/F/") || (f259 && request.1.contains("f-fedora.txt"));
            sessions += usize::from(request.0 == "POST");
            if !content || !of_the_file {
                wrong.push(format!("an upload nobody asked for: {request:?}"));
            }
        } else if users.contains(request) {
            own += 1;
        } else if f259 && request == &("PATCH".to_owned(), "me/drive/items/F".to_owned()) {
            known += 1;
        } else {
            wrong.push(format!("sent, and not the user's: {request:?}"));
        }
    }
    // The user's own request may be refused once (`412`: OneDrive changed
    // the item in the same listing) and is then sent again.
    if known > 1 || own > 2 || sessions > 3 {
        wrong.push(format!("sent more than once: the known request {known}, the user's {own}, upload sessions {sessions}"));
    }
    // OneDrive is as the listing left it, but for what the user did, the
    // content that waited, and the conflict copy that request ends in.
    let now = in_onedrive(&w);
    for (item, was) in &staged {
        match now.get(item) {
            None if case.local == 5 && item == of => {}
            None => wrong.push(format!("{item} is gone from OneDrive")),
            Some(is) => {
                if (&is.0, &is.1) != (&was.0, &was.1) && item != of {
                    wrong.push(format!("OneDrive has {item} at {:?}/{} and had it at {:?}/{}", is.0, is.1, was.0, was.1));
                }
                if is.2 != was.2 && !(content && item == "F") {
                    wrong.push(format!("the content of {item} changed in OneDrive"));
                }
            }
        }
    }
    let made: Vec<&String> = now.iter().filter(|(item, _)| !staged.contains_key(*item)).map(|(_, is)| &is.1).collect();
    if !(made.is_empty() || f259 && made == ["f-fedora.txt"]) {
        wrong.push(format!("made in OneDrive: {made:?}"));
    }
    if w.graph.with(|c| c.bin.keys().any(|binned| !(case.local == 5 && binned == of))) {
        wrong.push("something went to OneDrive's recycle bin".into());
    }
    // Nothing lost: what held data here before is in a file here or in OneDrive.
    let here: Vec<Vec<u8>> = on_disk.iter().filter_map(|o| std::fs::read(w.path(&o.0)).ok()).collect();
    for (at, data) in &data_before {
        let deleted_here = case.local == 5 && super::id_at(&w.path(at)).is_none() && !w.path(at).exists() && users.iter().any(|u| u.0 == "DELETE");
        if !here.contains(data) && !now.values().any(|is| &is.2 == data) && !deleted_here {
            wrong.push(format!("what {at} held is lost"));
        }
    }
    // I-b: the list is true. A line that says an item is still here names a
    // path where the object of an item that waits stands; what a line says
    // keeps it exists; and every item that waits is at or below such a line.
    let deferred = w.store.call(|s| s.deferred_ids()).await.unwrap();
    let mut places = BTreeMap::new();
    for item in &deferred {
        if let Some(at) = w.store.call({ let item = item.clone(); move |s| s.locate(konedrive_tree::Table::Items, &item) }).await.unwrap() {
            places.insert(item.clone(), at.rel);
        }
    }
    let mut lines = Vec::new();
    for line in w.store.call(|s| s.skipped()).await.unwrap() {
        if let Some(kept_by) = line.waits.as_ref().and_then(WaitsFor::path) {
            if std::fs::symlink_metadata(w.path(kept_by)).is_err() {
                wrong.push(format!("the skipped list says {:?}, where nothing is", line.waits));
            }
        }
        let Some(here) = line.here else { continue };
        let carries = id_at(&w.path(&here.display().to_string()));
        if !carries.as_ref().is_some_and(|carries| places.get(carries) == Some(&here)) {
            wrong.push(format!("the skipped list says an item is at {}, where {carries:?} stands", here.display()));
        }
        lines.push(here);
    }
    for (item, at) in &places {
        if !lines.iter().any(|line| at.starts_with(line)) {
            wrong.push(format!("{item} waits at {} and no line of the skipped list says so", at.display()));
        }
    }
    // Every item OneDrive has where the folder can hold it is here at that
    // place, or waits; every object here is its item's, at its place or
    // waiting; and no row is at a path where nothing stands.
    let held: Vec<(String, String)> = w.graph.with(|c| c.items.keys().filter(|item| item.as_str() != ROOT).filter_map(|item| Some((item.clone(), place_in_onedrive(c, item)?))).collect());
    for (item, path) in &held {
        if !on_disk.iter().any(|(at, carries)| at == path && carries == item) && !deferred.contains(item) {
            wrong.push(format!("{item} is {path} in OneDrive, is not there here and does not wait"));
        }
    }
    for (at, carries) in on_disk.iter().filter(|(_, carries)| !carries.is_empty()) {
        if on_disk.iter().filter(|(_, other)| other == carries).count() > 1 {
            wrong.push(format!("two objects carry {carries}"));
        }
        let waits = deferred.contains(carries) || lines.iter().any(|line| Path::new(at).starts_with(line));
        if !held.iter().any(|(item, path)| item == carries && path == at) && !waits {
            wrong.push(format!("{at} carries {carries}, is not where OneDrive has it and does not wait"));
        }
    }
    for row in w.store.call(|s| s.outbox_rows()).await.unwrap() {
        if !row.kind.removes() && std::fs::symlink_metadata(w.path(&row.rel.display().to_string())).is_err() {
            wrong.push(format!("a row {:?} at {}, where nothing is", row.kind, row.rel.display()));
        }
    }
    wrong
}

/// The gate. A small folder — `docs` with `f.txt`, `g.txt` and `sub/x.txt`,
/// `papers` with `p.txt`, and `top.txt` — and the combinations [`always`]
/// selects (the whole product runs them all) of: which item OneDrive takes out of what the folder can hold
/// (none, `docs`, `docs/sub`, `docs/f.txt`, `top.txt`), and how (a name too
/// long, a move into a folder that is not placed); one more change in the
/// same listing (none, a new file or a new folder at the freed name, an
/// existing item renamed to it, something moved out to the root, the folder
/// above renamed, two files exchanging names, two folders exchanging names,
/// a chain of three names, `docs/sub` moved to the root); a new version in
/// the same listing of a file downloaded here (none, `docs/sub/x.txt`,
/// `docs/g.txt`; with nothing done here); what was done here before (nothing, a new file
/// that cannot go up, a change of content, a rename, a rename with a
/// change, a delete); in a Changed and in a Full reconcile. Each must
/// settle, lose nothing, send nothing the user did not do, leave OneDrive
/// as the listing had it, say the truth on the skipped list, and never
/// leave the base with a version of a file the disk does not hold while
/// nothing waits for it ([`run`], [`base_ahead_of_disk`]).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_small_combination_of_an_unplaceable_item_another_change_and_local_work_keeps_the_invariants() {
    enumerate(false).await;
}

/// The whole product, which takes about a minute: every combination that
/// makes sense, where the test above runs the selection of [`always`].
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "about a minute: run with --ignored when remote/materialize or the upload steps change"]
async fn every_combination_the_whole_product() {
    enumerate(true).await;
}

/// The combinations that run with every run of the tests: each value of
/// each dimension at least once, each kind of second change with nothing
/// done here, each thing done here in the plain listings, and every
/// combination that has failed. The rest is the whole product's.
pub(super) fn always(case: Case) -> bool {
    let plain = case.other == 0;
    match (case.version, case.reason) {
        // A new version: in the plain listings, and where `docs/sub` is
        // moved out of a folder that leaves (which failed once).
        (1.., _) => (plain || case.other == SUB_OUT) && matches!(case.item, None | Some(0)),
        (_, 'M') => plain && (case.local == 0 || (case.local == 2 && case.item == Some(0))),
        // A name too long, or the chain: every second change with nothing
        // done here, and every thing done here in the plain listings, the
        // chain and the exchange of folders.
        _ => case.local == 0 || plain || (matches!(case.other, CHAIN | FOLDERS) && case.local <= 2),
    }
}

async fn enumerate(whole: bool) {
    let mut cases = Vec::new();
    for item in [None, Some(0), Some(1), Some(2), Some(3)] {
        for reason in if item.is_none() { vec!['N'] } else { vec!['N', 'M', 'R'] } {
            for other in 0..=SUB_OUT {
                for (local, version) in (0..6).map(|local| (local, 0)).chain([(0, 1), (0, 2)]) {
                    for full in [false, true] {
                        let case = Case { item, reason, other, local, version, full };
                        if !senseless(case) && (whole || always(case)) {
                            cases.push(case);
                        }
                    }
                }
            }
        }
    }
    let count = cases.len();
    // A dozen at a time: the rest of the suite runs beside this.
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

/// A file downloaded here is changed here, with its row recorded, and
/// OneDrive has a new version of it in the next listing: a plain file, and
/// one in a folder that can no longer be placed. Neither side overwrites
/// the other: what the user wrote ends in OneDrive, and the version
/// OneDrive had is still there. Nothing but uploads is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_version_and_an_edit_here_of_the_same_file_lose_neither() {
    for (leaves, full) in [(false, false), (false, true), (true, false), (true, true)] {
        let case = format!("leaves={leaves} full={full}");
        let w = Arc::new(World::read_write().await);
        let listing = w.listed().await;
        changed_here(&w, "docs/f.txt", "F").await;
        let theirs: &[u8] = b"a newer version in OneDrive";
        w.graph.with(|c| {
            c.edit("F", theirs);
            if leaves {
                c.rename("D", ROOT, &long_name());
            }
        });
        if full {
            listing.request_full();
        }
        let mut last = None;
        let mut settled = false;
        for _ in 0..8 {
            listing.cycle(&tokio_util::sync::CancellationToken::new()).await.unwrap_or_else(|e| panic!("{case}: {e}"));
            listing.join_replacements().await;
            w.examine_handed().await;
            w.upload().await;
            let now = (objects(&w), w.sent().len());
            settled = last.as_ref() == Some(&now);
            last = Some(now);
            if settled {
                break;
            }
        }
        assert!(settled, "{case}: does not settle: {last:?}");
        let in_onedrive: Vec<(String, Vec<u8>)> = w.graph.with(|c| c.items.values().map(|i| (i.name.clone(), i.content.clone())).collect());
        let names = |content: &[u8]| in_onedrive.iter().filter(|(_, has)| has == content).map(|(name, _)| name.len()).collect::<Vec<_>>();
        assert_eq!(names(b"changed here").len(), 1, "{case}: what was written here is in OneDrive, once: {:?}", w.sent());
        assert_eq!(names(theirs).len(), 1, "{case}: OneDrive's version is still there: {:?}", w.sent());
        assert!(w.graph.with(|c| c.bin.is_empty()), "{case}");
        let not_uploads: Vec<_> = w.sent().into_iter().filter(|request| !is_an_upload(request)).collect();
        assert_eq!(not_uploads, [], "{case}");
        assert_eq!(base_ahead_of_disk(&w).await, Vec::<String>::new(), "{case}");
    }
}
