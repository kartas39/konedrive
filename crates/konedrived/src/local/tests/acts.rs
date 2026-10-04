//! Every small combination of what the user does in a folder, what the store has on
//! record and how the examination looks at it, with the outcome asserted, not the steps.
//!
//! The folder: a downloaded file (`a.txt`, item A), a file not downloaded (`p.bin`, P), a
//! folder (`docs`, D) with a downloaded file (`docs/f.txt`, F). One act, then a second one
//! on the same object or beside it; the store with each item's object on record, or with
//! none (as after a rebuild); and three ways to look: a batch naming the names the acts
//! touched, a Full scan, and a Full scan in which the second act lands after the folder
//! was listed and before anything is decided.
//!
//! What must hold, whatever the combination:
//!
//! - the item is still the item (its object keeps its marks, and the store records that
//!   object), unless the user removed or replaced it;
//! - the rows are exactly those the acts call for: no `delete`, `move` or `move-out` the
//!   user did not make, one `create` for a copy with content;
//! - nothing with data leaves the disk; an empty copy is removed only when the item's own
//!   recorded object was seen (listed) in the same run, and that is said in Activity;
//! - a second and a third look change nothing.
//!
//! The first look may be incomplete (it asks for another look, or the second act came
//! after its listing), never wrong: it is checked for safety only, the look after it for
//! the exact outcome. A model of the folder's objects (which one carries which id, which
//! one the listing placed) says what the outcome is, by the rule of `docs/limitations/F53.md`,
//! from the acts and from what the store knew before the look; nothing the examination
//! wrote is read back to decide what it should have written.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use super::copies::{copy_as_it_is, copy_tree};
use super::passed_over::scan_changing;
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    Nothing,
    Edit,
    Rename,
    MoveIn,
    Delete,
    CopyFile,
    CopyPlaceholder,
    CopyFolder,
    HardLink,
    SaveByRename,
    NewFile,
    NewIgnored,
}

const ACTS: [Act; 12] = [
    Act::Nothing,
    Act::Edit,
    Act::Rename,
    Act::MoveIn,
    Act::Delete,
    Act::CopyFile,
    Act::CopyPlaceholder,
    Act::CopyFolder,
    Act::HardLink,
    Act::SaveByRename,
    Act::NewFile,
    Act::NewIgnored,
];

/// The second act.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Then {
    Nothing,
    /// `docs/f.txt` gets other content.
    EditSibling,
    /// `p.bin` is removed.
    DeleteSibling,
    /// A new file beside the rest.
    NewBeside,
    /// What the first act was about is removed: the item's own object under its first
    /// name (the original of a copy; of a copied folder, the folder), or the new file.
    DeleteIt,
}

const THEN: [Then; 5] = [Then::Nothing, Then::EditSibling, Then::DeleteSibling, Then::NewBeside, Then::DeleteIt];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    Named,
    Full,
    /// A Full scan; the second act comes after the folder's own directory was listed.
    Interrupted,
}

const LOOKS: [Look; 3] = [Look::Named, Look::Full, Look::Interrupted];

/// The items of the listing: id, place, whether a folder.
const ITEMS: [(&str, &str, bool); 4] = [("A", "a.txt", false), ("P", "p.bin", false), ("D", "docs", true), ("F", "docs/f.txt", false)];

/// One object on disk, as the model has it.
#[derive(Debug, Clone)]
struct Object {
    /// Its names, relative to the folder.
    names: Vec<String>,
    /// The id mark it carries.
    id: Option<&'static str>,
    /// Which object it is, whatever its names: the model's own number for it.
    key: usize,
    /// The object the listing placed for that id.
    own: bool,
    dir: bool,
    /// A file's content; `None` for one not downloaded.
    data: Option<Vec<u8>>,
    edited: bool,
}

fn object(name: &str, id: Option<&'static str>, own: bool, data: Option<&[u8]>) -> Object {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let key = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Object { names: vec![name.to_owned()], id, key, own, dir: false, data: data.map(<[u8]>::to_vec), edited: false }
}

struct Scene<'f> {
    fx: &'f Fx,
    objects: Vec<Object>,
    /// The names the acts touched: what a batch of names names.
    touched: Vec<(String, String)>,
}

/// The folder, placed; with the store's record of each object, or without.
fn placed(handles: bool) -> Fx {
    let fx = Fx::new(&[folder("D", "R", "docs"), file("A", "R", "a.txt", b"abc"), file("P", "R", "p.bin", b"only in the cloud"), file("F", "D", "f.txt", b"ff")]);
    fx.hydrate("a.txt", b"abc");
    fx.hydrate("docs/f.txt", b"ff");
    if !handles {
        fx.store.call_blocking(|s| s.forget_local_handles()).unwrap();
    }
    fx
}

impl<'f> Scene<'f> {
    fn new(fx: &'f Fx) -> Self {
        let objects = vec![
            object("a.txt", Some("A"), true, Some(b"abc")),
            object("p.bin", Some("P"), true, None),
            Object { dir: true, ..object("docs", Some("D"), true, None) },
            object("docs/f.txt", Some("F"), true, Some(b"ff")),
        ];
        Scene { fx, objects, touched: Vec::new() }
    }

    fn touch(&mut self, rel: &str) {
        let rel = Path::new(rel);
        self.touched.push((rel.parent().unwrap().display().to_string(), rel.file_name().unwrap().to_string_lossy().into_owned()));
    }

    fn own(&self, id: &str) -> Option<usize> {
        self.objects.iter().position(|o| o.own && o.id == Some(id))
    }

    /// `rm -r name`.
    fn remove(&mut self, name: &str) {
        let path = self.fx.path(name);
        if path.is_dir() {
            std::fs::remove_dir_all(path).unwrap();
        } else {
            std::fs::remove_file(path).unwrap();
        }
        let below = format!("{name}/");
        for o in &mut self.objects {
            o.names.retain(|n| n != name && !n.starts_with(&below));
        }
        self.objects.retain(|o| !o.names.is_empty());
        self.touch(name);
    }

    fn rename(&mut self, from: &str, to: &str) {
        self.fx.rename(from, to);
        for name in self.objects.iter_mut().flat_map(|o| o.names.iter_mut()).filter(|n| *n == from) {
            *name = to.to_owned();
        }
        self.touch(from);
        self.touch(to);
    }

    /// Other content, of another size, in the same object.
    fn edit(&mut self, name: &str, content: &[u8]) {
        self.fx.write(name, content);
        let o = self.objects.iter_mut().find(|o| o.names.iter().any(|n| n == name)).unwrap();
        o.data = Some(content.to_vec());
        o.edited = true;
        self.touch(name);
    }

    fn create(&mut self, name: &str, content: &[u8]) {
        self.fx.write(name, content);
        self.objects.push(object(name, None, false, Some(content)));
        self.touch(name);
    }

    fn act(&mut self, act: Act) {
        match act {
            Act::Nothing => {}
            Act::Edit => self.edit("a.txt", b"edited here"),
            Act::Rename => self.rename("a.txt", "b.txt"),
            Act::MoveIn => self.rename("a.txt", "docs/a.txt"),
            Act::Delete => self.remove("a.txt"),
            Act::CopyFile => {
                copy_keeping_attributes(&self.fx.path("a.txt"), &self.fx.path("a2.txt"));
                self.objects.push(object("a2.txt", Some("A"), false, Some(b"abc")));
                self.touch("a2.txt");
            }
            Act::CopyPlaceholder => {
                copy_as_it_is(&self.fx.path("p.bin"), &self.fx.path("p2.bin"));
                self.objects.push(object("p2.bin", Some("P"), false, None));
                self.touch("p2.bin");
            }
            Act::CopyFolder => {
                copy_tree(&self.fx.path("docs"), &self.fx.path("docs2"));
                self.objects.push(Object { dir: true, ..object("docs2", Some("D"), false, None) });
                self.objects.push(object("docs2/f.txt", Some("F"), false, Some(b"ff")));
                self.touch("docs2");
            }
            Act::HardLink => {
                std::fs::hard_link(self.fx.path("a.txt"), self.fx.path("a2.txt")).unwrap();
                let a = self.own("A").unwrap();
                self.objects[a].names.push("a2.txt".into());
                self.touch("a2.txt");
            }
            Act::SaveByRename => {
                self.create("a.txt.new", b"the new text");
                self.objects.retain(|o| o.names != ["a.txt"]);
                self.rename("a.txt.new", "a.txt");
            }
            Act::NewFile => self.create("n.txt", b"new"),
            Act::NewIgnored => self.create("n.txt~", b"a backup"),
        }
    }

    fn then(&mut self, act: Act, then: Then) {
        match then {
            Then::Nothing => {}
            Then::EditSibling => self.edit("docs/f.txt", b"edited there too"),
            Then::DeleteSibling => self.remove("p.bin"),
            Then::NewBeside => self.create("z.txt", b"beside"),
            Then::DeleteIt => {
                let name = match act {
                    Act::Delete => return,
                    Act::CopyPlaceholder => "p.bin".to_owned(),
                    Act::CopyFolder => "docs".to_owned(),
                    Act::SaveByRename => "a.txt".to_owned(),
                    Act::NewFile => "n.txt".to_owned(),
                    Act::NewIgnored => "n.txt~".to_owned(),
                    _ => self.objects[self.own("A").unwrap()].names[0].clone(),
                };
                self.remove(&name);
            }
        }
    }

    fn named(&self) -> Batch {
        let mut batch = Batch::new();
        for (dir, name) in &self.touched {
            batch.name(Path::new(dir), OsStr::new(name));
        }
        batch
    }

    fn skipped(&self) -> BTreeSet<(String, String)> {
        self.fx.store.call_blocking(move |s| s.local_skipped()).unwrap().into_iter().map(|s| (s.rel.display().to_string(), s.reason.to_string())).collect()
    }

    fn rows(&self) -> Vec<(OutboxKind, String, Option<String>)> {
        let mut rows = self.fx.summary();
        rows.sort_by(|a, b| (&a.1, &a.2, a.0 as u8).cmp(&(&b.1, &b.2, b.0 as u8)));
        rows
    }

    fn said(&self) -> usize {
        self.fx.store.call_blocking(move |s| s.recent_activity(50)).unwrap().len()
    }
}

/// What the acts call for.
#[derive(Debug, Default)]
struct Outcome {
    rows: Vec<(OutboxKind, String, Option<String>)>,
    list: BTreeSet<(String, String)>,
    /// Items that are still items: id, and the name their object stands at.
    items: Vec<(&'static str, String)>,
    /// Names whose marks come off.
    stripped: Vec<String>,
    /// Empty copies that go.
    removed: Vec<String>,
}

/// What the store knows of the items when a look begins, as the model has it: the object
/// on record for each (none after a rebuild, until a look finds the item and records what
/// it found), and where each is expected: its place in the listing, or where a waiting row
/// last saw it. Nothing here is read from the store.
#[derive(Clone)]
struct Known {
    /// Item id → the object's number in the model.
    record: BTreeMap<&'static str, usize>,
    expect: BTreeMap<&'static str, Expected>,
}

#[derive(Clone)]
enum Expected {
    At(String),
    /// A row says it left; the row is at this place.
    Removed(String),
}

/// The object at `name`, if one is there.
fn handle_of(fx: &Fx, name: &str) -> Option<FileHandle> {
    let path = fx.path(name);
    File::open(path.parent()?).and_then(|dir| FileHandle::at(&dir, path.file_name().unwrap())).ok()
}

/// What the store knows before any look: every placed object on record, or none.
fn known_at_first(objects: &[Object], handles: bool) -> Known {
    let record = objects.iter().filter(|o| handles && o.own).map(|o| (o.id.unwrap(), o.key)).collect();
    Known { record, expect: ITEMS.iter().map(|(id, place, _)| (*id, Expected::At(place.to_string()))).collect() }
}

/// The objects as a Full scan listed them when the second act came after the folder's own
/// directory was read and before the directories below it were: the names directly in
/// the folder as they were `before` that act, the names below as they are `after` it.
fn listed_around(before: &[Object], after: &[Object]) -> Vec<Object> {
    let top = |name: &String| !name.contains('/');
    let mut seen: Vec<Object> = Vec::new();
    for old in before {
        let now = after.iter().find(|o| o.key == old.key);
        let mut o = now.unwrap_or(old).clone();
        o.names = old.names.iter().filter(|n| top(n)).cloned().collect();
        o.names.extend(now.into_iter().flat_map(|o| o.names.iter().filter(|n| !top(n)).cloned()));
        seen.push(o);
    }
    for new in after.iter().filter(|o| !before.iter().any(|old| old.key == o.key)) {
        let mut o = new.clone();
        o.names.retain(|n| !top(n));
        seen.push(o);
    }
    seen.retain(|o| !o.names.is_empty());
    seen
}

/// The outcome by the one rule of identity: an item's object is the one the store records;
/// with that one not there, or none recorded, the one standing at the item's place; every
/// other object carrying the id is a copy. A missing item is deleted only on the evidence
/// of its recorded object, and a folder only when everything in it has one. An empty copy
/// goes only when the item's object is the recorded one. Also what the store knows after
/// such a look: the object found for an item is on record, and a row says where the item
/// was last seen. `unread`: directories the look did not get to read (gone by the time it
/// came to them): nothing expected inside them is judged.
fn outcome(fx: &Fx, objects: &[Object], known: &Known, unread: &[String]) -> (Outcome, Known) {
    let mut out = Outcome::default();
    let mut next = known.clone();
    let ignored = |name: &str| fx.ignore.matches(Path::new(name).file_name().unwrap());
    let mut taken: Vec<usize> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    for (id, base, is_dir) in ITEMS {
        let (place, removed) = match &known.expect[id] {
            Expected::At(place) => (place.as_str(), None),
            Expected::Removed(at) => (base, Some(at.clone())),
        };
        let carriers: Vec<usize> = (0..objects.len()).filter(|&i| objects[i].id == Some(id) && objects[i].dir == is_dir).collect();
        let recorded = known.record.get(id).and_then(|key| carriers.iter().copied().find(|&i| objects[i].key == *key));
        let item = recorded.or_else(|| carriers.iter().copied().find(|&i| objects[i].names.iter().any(|n| n == place)));
        let mut copies: Vec<(usize, bool)> = Vec::new();
        match item {
            Some(i) => {
                let o = &objects[i];
                let at = if o.names.iter().any(|n| n == place) { place.to_owned() } else { o.names.iter().min().unwrap().clone() };
                out.list.extend(o.names.iter().filter(|n| **n != at && !ignored(n)).map(|n| (n.clone(), "hard-link".to_owned())));
                if o.edited {
                    out.rows.push((Update, at.clone(), Some(id.into())));
                } else if at != base {
                    out.rows.push((Move, at.clone(), Some(id.into())));
                }
                next.record.insert(id, o.key);
                next.expect.insert(id, Expected::At(at.clone()));
                out.items.push((id, at));
                copies.extend(carriers.iter().filter(|&&c| c != i).map(|&c| (c, recorded == Some(i))));
            }
            None if unread.iter().any(|dir| Path::new(place).starts_with(dir)) => {}
            None => {
                let newcomer = (0..objects.len()).find(|&n| !is_dir && removed.is_none() && objects[n].id.is_none() && !objects[n].dir && objects[n].names.iter().any(|n| n == place));
                if let Some(n) = newcomer {
                    out.rows.push((Update, place.into(), Some(id.into())));
                    taken.push(n);
                } else if let Some(at) = removed {
                    if is_dir {
                        deleted.push(at.clone());
                    }
                    out.rows.push((Delete, at, Some(id.into())));
                } else if known.record.contains_key(id) && (!is_dir || known.record.contains_key("F")) && !deleted.iter().any(|dir| Path::new(place).starts_with(dir)) {
                    out.rows.push((Delete, place.into(), Some(id.into())));
                    next.expect.insert(id, Expected::Removed(place.to_owned()));
                    if is_dir {
                        deleted.push(place.to_owned());
                    }
                }
                copies.extend(carriers.iter().map(|&c| (c, false)));
            }
        }
        for (c, certain) in copies {
            let o = &objects[c];
            match (&o.data, o.dir) {
                (_, true) => {
                    out.stripped.push(o.names[0].clone());
                    out.rows.push((Mkdir, o.names[0].clone(), None));
                }
                (Some(_), _) if o.names.len() > 1 => out.list.extend(o.names.iter().map(|n| (n.clone(), "hard-link".to_owned()))),
                (Some(_), _) => {
                    out.stripped.push(o.names[0].clone());
                    out.rows.push((Create, o.names[0].clone(), None));
                }
                (None, _) if certain && o.names.len() == 1 => out.removed.push(o.names[0].clone()),
                (None, _) => out.list.extend(o.names.iter().map(|n| (n.clone(), "not-downloaded".to_owned()))),
            }
        }
    }
    for (i, o) in objects.iter().enumerate() {
        if o.id.is_none() && !taken.contains(&i) && !ignored(&o.names[0]) {
            out.rows.push((if o.dir { Mkdir } else { Create }, o.names[0].clone(), None));
        }
    }
    out.rows.sort_by(|a, b| (&a.1, &a.2, a.0 as u8).cmp(&(&b.1, &b.2, b.0 as u8)));
    (out, next)
}

/// What is wrong on disk: nothing with data went or changed, and no object of the model
/// went but the empty copies that may.
fn disk(scene: &Scene, want: &Outcome, exact: bool, records: bool) -> Vec<String> {
    let mut wrong = Vec::new();
    for o in &scene.objects {
        for name in &o.names {
            let path = scene.fx.path(name);
            let may_go = want.removed.contains(name);
            match (&o.data, path.symlink_metadata().is_ok()) {
                (_, false) if may_go => {}
                (_, false) => wrong.push(format!("{name} is gone from the disk")),
                (Some(data), true) if std::fs::read(&path).unwrap() != *data => wrong.push(format!("{name} lost its content")),
                (None, true) if exact && may_go => wrong.push(format!("the empty copy {name} is still there")),
                _ => {}
            }
        }
    }
    for (id, at) in &want.items {
        if id_of(&scene.fx.path(at)).as_deref() != Some(*id) {
            wrong.push(format!("{at} no longer carries {id}"));
        }
    }
    if exact {
        for (id, at) in want.items.iter().filter(|_| records) {
            let recorded = scene.fx.store.call_blocking({ let id = id.to_string(); move |s| s.local_handle(&id) }).unwrap();
            if recorded.is_none() || recorded != handle_of(scene.fx, at) {
                wrong.push(format!("the store does not record the object at {at} for {id}"));
            }
        }
        for name in &want.stripped {
            if id_of(&scene.fx.path(name)).is_some() {
                wrong.push(format!("{name} still carries an item's id"));
            }
        }
    }
    wrong
}

/// One combination: what is wrong with it, if anything.
fn run(act: Act, then: Then, handles: bool, look: Look) -> Vec<String> {
    let fx = placed(handles);
    let scene = std::cell::RefCell::new(Scene::new(&fx));
    let mut wrong = Vec::new();
    let at_first = known_at_first(&scene.borrow().objects, handles);
    scene.borrow_mut().act(act);
    let after_first_act = scene.borrow().objects.clone();
    // What is alive is alive where it stands once both acts are made: the helper's answer.
    let second_act = || {
        scene.borrow_mut().then(act, then);
        fx.liveness.alive_tree(&fx.root.path);
    };
    let first = if look == Look::Interrupted {
        let done = Cell::new(false);
        scan_changing(&fx, |_| {
            if !done.replace(true) {
                second_act();
            }
        })
        .unwrap()
    } else {
        second_act();
        let batch = if look == Look::Named { scene.borrow().named() } else { Batch::full() };
        fx.examine(&batch)
    };
    let mut scene = scene.into_inner();
    // What the first look calls for, by the model alone: from what the store knew before
    // it (every placed object on record, or none) and the objects as it listed them (as
    // they are, or, interrupted, the folder's own directory as it was before the second
    // act). It may do less than that (it acts only on what still stands), never anything
    // else.
    let listed = if look == Look::Interrupted { listed_around(&after_first_act, &scene.objects) } else { scene.objects.clone() };
    let unread: Vec<String> = listed.iter().filter(|o| o.dir && !scene.objects.iter().any(|now| now.key == o.key)).map(|o| o.names[0].clone()).collect();
    let (first_calls_for, then_known) = outcome(&fx, &listed, &at_first, &unread);
    // An empty copy that look removed: it had listed the item's own recorded object (which
    // the second act may have removed since: seen in that run all the same).
    let went = first_calls_for.removed.clone();
    scene.objects.retain(|o| !went.contains(&o.names[0]));
    // Whether a look records every item it finds: a batch of names finds only those named.
    let records = handles || look != Look::Named;
    // The outcome: the objects as they are after both acts, and what the model says the
    // store knows after the first look.
    let (want, _) = outcome(&fx, &scene.objects, &then_known, &[]);
    for row in scene.rows() {
        if matches!(row.0, Delete | Move | MoveOut) && !first_calls_for.rows.contains(&row) {
            wrong.push(format!("the first look made the row {row:?}"));
        }
    }
    wrong.extend(disk(&scene, &want, false, false).into_iter().map(|w| format!("after the first look, {w}")));
    // The look after it: the exact outcome.
    let again = |after: &Examined| {
        let mut batch = if look == Look::Named { scene.named() } else { Batch::full() };
        batch.merge(after.recheck.clone());
        batch.merge(after.passed.clone());
        scene.fx.examine(&batch)
    };
    let second = again(&first);
    let settled = (scene.rows(), scene.skipped(), scene.said());
    if settled.0 != want.rows {
        wrong.push(format!("the rows are {:?}, not {:?}", settled.0, want.rows));
    }
    if settled.1 != want.list {
        wrong.push(format!("the list is {:?}, not {:?}", settled.1, want.list));
    }
    if settled.2 != want.removed.len() + went.len() {
        wrong.push(format!("Activity has {} line(s) for {} empty copies removed", settled.2, want.removed.len() + went.len()));
    }
    wrong.extend(disk(&scene, &want, true, records));
    wrong.extend(went.iter().filter(|name| fx.path(name).exists()).map(|name| format!("the empty copy {name} is still there")));
    // And once more: nothing changes.
    again(&second);
    let third = (scene.rows(), scene.skipped(), scene.said());
    if third != settled {
        wrong.push(format!("a third look changes it: {third:?}"));
    }
    wrong.extend(disk(&scene, &want, true, records).into_iter().map(|w| format!("after the third look, {w}")));
    wrong
}

#[test]
fn every_small_combination_of_two_local_acts_the_record_and_the_look_has_the_outcome_the_acts_call_for() {
    let mut wrong = Vec::new();
    let mut ran = 0;
    for act in ACTS {
        for then in THEN {
            for handles in [true, false] {
                for look in LOOKS {
                    ran += 1;
                    wrong.extend(run(act, then, handles, look).into_iter().map(|what| format!("{act:?}, then {then:?}, handles={handles}, {look:?}: {what}")));
                }
            }
        }
    }
    assert_eq!(ran, 360);
    assert!(wrong.is_empty(), "{} wrong:\n{}", wrong.len(), wrong.join("\n"));
}
