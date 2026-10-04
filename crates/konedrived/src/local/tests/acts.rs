//! Every small combination of what the user does in a folder, what the store has on
//! record and how the examination looks at it, with the outcome asserted, not the steps.
//!
//! The folder: a downloaded file (`a.txt`, item A), a file not downloaded (`p.bin`, P), a
//! folder (`docs`, D) with a downloaded file (`docs/f.txt`, F), an empty folder (`keep`, K). One act, then a second one
//! on the same object or beside it; the store with each item's object on record, or with
//! none (as after a rebuild); and three ways to look: a batch naming the names the acts
//! touched, a Full scan, and a Full scan in which the second act lands after the folder
//! was listed and before anything is decided. Some acts move an object out of the folder,
//! to a directory beside it; some are not seen where they end (a move whose second name
//! no batch names), so that only asking after the object finds it.
//!
//! What must hold, whatever the combination:
//!
//! - the item is still the item (its object keeps its marks, and the store records that
//!   object), unless the user removed or replaced it;
//! - the rows are exactly those the acts call for: no `delete`, `move` or `move-out` the
//!   user did not make, one `create` for a copy with content; a folder that went is one
//!   row, in front of which goes the `move-out` of what left it first; nothing is said of
//!   an item that is alive in the folder where the look did not look, nor of the folder
//!   it was in, until it is found;
//! - nothing with data leaves the disk; an empty copy is removed only when the item's own
//!   recorded object was seen (listed) in the same run, and that is said in Activity;
//! - a file of an item whose marks are damaged is listed as not uploaded, and its content is
//!   never sent;
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
    /// `a.txt` goes into `docs`, and only the name it left is named.
    MoveInUnseen,
    /// `a.txt` goes out of the folder.
    MoveOut,
    /// `rm -r docs`.
    DeleteFolder,
    /// `docs` goes out of the folder, with what is in it.
    MoveOutFolder,
    /// `docs/f.txt` goes out of the folder, and nothing is named.
    DragOut,
    /// `docs/f.txt` goes into `keep`, and nothing is named.
    LeaveUnseen,
    /// `a.txt` loses its state mark: no state konedrive can read.
    DamageMarks,
}

const ACTS: [Act; 19] = [
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
    Act::MoveInUnseen,
    Act::MoveOut,
    Act::DeleteFolder,
    Act::MoveOutFolder,
    Act::DragOut,
    Act::LeaveUnseen,
    Act::DamageMarks,
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
    /// name (the original of a copy; of a copied folder, the folder), or the new file;
    /// where it stands now, outside the folder too.
    DeleteIt,
    /// `rm -r docs`, if it is in the folder.
    DeleteFolder,
}

const THEN: [Then; 6] = [Then::Nothing, Then::EditSibling, Then::DeleteSibling, Then::NewBeside, Then::DeleteIt, Then::DeleteFolder];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    Named,
    Full,
    /// A Full scan; the second act comes after the folder's own directory was listed.
    Interrupted,
}

const LOOKS: [Look; 3] = [Look::Named, Look::Full, Look::Interrupted];

/// The items of the listing: id, place, whether a folder.
const ITEMS: [(&str, &str, bool); 5] = [("A", "a.txt", false), ("P", "p.bin", false), ("D", "docs", true), ("F", "docs/f.txt", false), ("K", "keep", true)];

/// One object on disk, as the model has it.
#[derive(Debug, Clone)]
struct Object {
    /// Its names, relative to the folder; of one that left the folder (`out`), to the
    /// directory beside it.
    names: Vec<String>,
    out: bool,
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
    /// A file with an id whose state mark cannot be read.
    damaged: bool,
}

fn object(name: &str, id: Option<&'static str>, own: bool, data: Option<&[u8]>) -> Object {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let key = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Object { names: vec![name.to_owned()], out: false, id, key, own, dir: false, data: data.map(<[u8]>::to_vec), edited: false, damaged: false }
}

struct Scene<'f> {
    fx: &'f Fx,
    objects: Vec<Object>,
    /// The names the acts touched: what a batch of names names.
    touched: Vec<(String, String)>,
    /// Objects moved where no batch names them: a batch of names lists one only once its
    /// name is named.
    unseen: Vec<usize>,
}

/// The folder, placed; with the store's record of each object, or without.
fn placed(handles: bool) -> Fx {
    let fx = Fx::new(&[folder("D", "R", "docs"), folder("K", "R", "keep"), file("A", "R", "a.txt", b"abc"), file("P", "R", "p.bin", b"only in the cloud"), file("F", "D", "f.txt", b"ff")]);
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
            Object { dir: true, ..object("keep", Some("K"), true, None) },
        ];
        Scene { fx, objects, touched: Vec::new(), unseen: Vec::new() }
    }

    fn touch(&mut self, rel: &str) {
        let rel = Path::new(rel);
        self.touched.push((rel.parent().unwrap().display().to_string(), rel.file_name().unwrap().to_string_lossy().into_owned()));
    }

    fn own(&self, id: &str) -> Option<usize> {
        self.objects.iter().position(|o| o.own && o.id == Some(id))
    }

    fn path(&self, name: &str, out: bool) -> PathBuf {
        if out { self.fx.outside.join(name) } else { self.fx.path(name) }
    }

    /// `rm -r name`.
    fn remove(&mut self, name: &str) {
        self.remove_at(name, false);
        self.touch(name);
    }

    /// `rm -r name`, in the folder or (`out`) beside it.
    fn remove_at(&mut self, name: &str, out: bool) {
        let path = self.path(name, out);
        if path.is_dir() {
            std::fs::remove_dir_all(path).unwrap();
        } else {
            std::fs::remove_file(path).unwrap();
        }
        let below = format!("{name}/");
        for o in self.objects.iter_mut().filter(|o| o.out == out) {
            o.names.retain(|n| n != name && !n.starts_with(&below));
        }
        self.objects.retain(|o| !o.names.is_empty());
    }

    fn rename(&mut self, from: &str, to: &str) {
        let unseen = self.unseen.len();
        self.rename_unseen(from, to);
        self.unseen.truncate(unseen);
        self.touch(from);
        self.touch(to);
    }

    /// A rename no batch names: the object is not seen where it is now.
    fn rename_unseen(&mut self, from: &str, to: &str) {
        self.fx.rename(from, to);
        for o in self.objects.iter_mut().filter(|o| !o.out) {
            for name in o.names.iter_mut().filter(|n| *n == from) {
                *name = to.to_owned();
                self.unseen.push(o.key);
            }
        }
    }

    /// `name` goes out of the folder, to the directory beside it, with what is below it.
    fn move_out(&mut self, name: &str) {
        let to = Path::new(name).file_name().unwrap().to_string_lossy().into_owned();
        std::fs::rename(self.fx.path(name), self.fx.outside.join(&to)).unwrap();
        let below = format!("{name}/");
        for o in self.objects.iter_mut().filter(|o| o.names.iter().any(|n| n == name || n.starts_with(&below))) {
            assert_eq!(o.names.len(), 1);
            o.names[0] = format!("{to}{}", &o.names[0][name.len()..]);
            o.out = true;
        }
    }

    /// Other content, of another size, in the same object.
    fn edit(&mut self, name: &str, content: &[u8]) {
        let o = self.objects.iter_mut().find(|o| o.names.iter().any(|n| n == name)).unwrap();
        o.data = Some(content.to_vec());
        o.edited = true;
        let out = o.out;
        std::fs::write(self.path(name, out), content).unwrap();
        if !out {
            self.touch(name);
        }
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
            Act::MoveInUnseen => {
                self.rename_unseen("a.txt", "docs/a.txt");
                self.touch("a.txt");
            }
            Act::MoveOut => {
                self.move_out("a.txt");
                self.touch("a.txt");
            }
            Act::DeleteFolder => self.remove("docs"),
            Act::MoveOutFolder => {
                self.move_out("docs");
                self.touch("docs");
            }
            Act::DragOut => self.move_out("docs/f.txt"),
            Act::LeaveUnseen => self.rename_unseen("docs/f.txt", "keep/f.txt"),
            Act::DamageMarks => {
                xattr::remove(self.fx.path("a.txt"), placeholder::XATTR_STATE).unwrap();
                let a = self.own("A").unwrap();
                self.objects[a].damaged = true;
                self.touch("a.txt");
            }
        }
    }

    fn then(&mut self, act: Act, then: Then) {
        match then {
            Then::Nothing => {}
            Then::EditSibling => {
                // Where it stands now; nothing if it went with its folder.
                if let Some(f) = self.own("F") {
                    let name = self.objects[f].names[0].clone();
                    self.edit(&name, b"edited there too");
                }
            }
            Then::DeleteSibling => self.remove("p.bin"),
            Then::NewBeside => self.create("z.txt", b"beside"),
            Then::DeleteFolder => {
                if self.fx.path("docs").is_dir() {
                    self.remove("docs");
                }
            }
            Then::DeleteIt => {
                let own = |id: &str| self.own(id).map(|i| (self.objects[i].names[0].clone(), self.objects[i].out));
                let it = match act {
                    Act::Delete | Act::DeleteFolder => None,
                    Act::CopyPlaceholder => own("P"),
                    Act::CopyFolder | Act::MoveOutFolder => own("D"),
                    Act::DragOut | Act::LeaveUnseen => own("F"),
                    Act::SaveByRename => Some(("a.txt".to_owned(), false)),
                    Act::NewFile => Some(("n.txt".to_owned(), false)),
                    Act::NewIgnored => Some(("n.txt~".to_owned(), false)),
                    _ => own("A"),
                };
                match it {
                    Some((name, false)) => self.remove(&name),
                    Some((name, true)) => self.remove_at(&name, true),
                    None => {}
                }
            }
        }
    }

    /// The objects in the folder a look can list: all of them, but for a batch of names
    /// those moved where it does not look and that nothing `found` since.
    fn listed(&self, look: Look, found: &[usize]) -> Vec<Object> {
        let looked = looked_at(&self.touched, &self.objects);
        let hidden = |o: &Object| look == Look::Named && self.unseen.contains(&o.key) && !found.contains(&o.key) && !o.names.iter().any(|n| looked(n));
        self.objects.iter().filter(|o| !o.out && !hidden(o)).cloned().collect()
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

/// Where a batch naming the names `touched` looks: at each name, and at the whole of a
/// directory in which a name it names is not there (a delete, a rename's old side).
fn looked_at(touched: &[(String, String)], objects: &[Object]) -> impl Fn(&str) -> bool {
    let rel = |(dir, name): &(String, String)| if dir.is_empty() { name.clone() } else { format!("{dir}/{name}") };
    let there = |rel: &String| objects.iter().any(|o| !o.out && o.names.contains(rel));
    let whole: BTreeSet<String> = touched.iter().filter(|t| !there(&rel(t))).map(|t| t.0.clone()).collect();
    let named: BTreeSet<String> = touched.iter().map(rel).collect();
    move |place: &str| named.contains(place) || whole.contains(&Path::new(place).parent().unwrap().display().to_string())
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
    /// Objects found by asking after them: alive in the folder where the look did not
    /// look. The look after it is told to look there.
    found: Vec<usize>,
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
    /// A row says it left (a `delete` or a `move-out`); the row is at this place.
    Removed(OutboxKind, String),
}

/// What became of an item no listed object is.
enum Missing {
    /// The store records no object for it: nothing proves anything.
    NoRecord,
    /// Its recorded object is alive in the folder, where the look did not look.
    Elsewhere(usize),
    /// Its recorded object is alive outside the folder, at this name there.
    Outside(String),
    Gone,
}

fn missing(id: &str, known: &Known, alive: &[Object]) -> Missing {
    let Some(key) = known.record.get(id) else { return Missing::NoRecord };
    match alive.iter().find(|o| o.key == *key) {
        None => Missing::Gone,
        Some(o) if o.out => Missing::Outside(o.names[0].clone()),
        Some(o) => Missing::Elsewhere(o.key),
    }
}

/// What a look can know of the folder.
struct Seen<'a> {
    /// The objects it listed.
    listed: &'a [Object],
    /// Every object there is when it decides, in the folder and outside: what the helper
    /// answers from.
    alive: &'a [Object],
    /// Directories it did not get to read (gone by the time it came to them): nothing
    /// expected inside them is judged.
    unread: &'a [String],
    /// The names a batch of names named; `None` for a Full scan, which looks everywhere.
    named: Option<&'a dyn Fn(&str) -> bool>,
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
    for old in before.iter().filter(|o| !o.out) {
        let now = after.iter().find(|o| o.key == old.key);
        let mut o = now.unwrap_or(old).clone();
        o.names = old.names.iter().filter(|n| top(n)).cloned().collect();
        o.names.extend(now.into_iter().flat_map(|o| o.names.iter().filter(|n| !top(n)).cloned()));
        seen.push(o);
    }
    for new in after.iter().filter(|o| !o.out && !before.iter().any(|old| old.key == o.key)) {
        let mut o = new.clone();
        o.names.retain(|n| !top(n));
        seen.push(o);
    }
    seen.retain(|o| !o.names.is_empty());
    seen
}

/// The outcome by the one rule of identity: an item's object is the one the store records;
/// with that one not there, or none recorded, the one standing at the item's place; every
/// other object carrying the id is a copy. An item that is not found is judged only where
/// the look looked: at a place it named, or inside a folder that went. It is judged by its
/// recorded object alone: gone, a `delete`; alive outside the folder, a `move-out`; alive
/// in the folder, nothing yet, and the look after it looks there; none recorded, nothing.
/// A folder goes as one row, and only when everything in it is decided: what went with it
/// (gone, or where the folder is now) has no row, what left it first has its own
/// `move-out`, what is elsewhere in the folder or has no record keeps the folder. An empty
/// copy goes only when the item's object is the recorded one. Also what the store knows
/// after such a look: the object found for an item is on record, and a row says where the
/// item was last seen.
fn outcome(fx: &Fx, seen: &Seen, known: &Known) -> (Outcome, Known) {
    let objects = seen.listed;
    let mut out = Outcome::default();
    let mut next = known.clone();
    let ignored = |name: &str| fx.ignore.matches(Path::new(name).file_name().unwrap());
    let under = |place: &str, dirs: &[String]| dirs.iter().any(|dir| Path::new(place).starts_with(dir));
    let find = |id: &str, place: &str, is_dir: bool| {
        let carriers: Vec<usize> = (0..objects.len()).filter(|&i| objects[i].id == Some(id) && objects[i].dir == is_dir).collect();
        let recorded = known.record.get(id).and_then(|key| carriers.iter().copied().find(|&i| objects[i].key == *key));
        let item = recorded.or_else(|| carriers.iter().copied().find(|&i| objects[i].names.iter().any(|n| n == place)));
        (carriers, recorded, item)
    };
    let mut taken: Vec<usize> = Vec::new();
    // Folders that left: nothing whose place is inside one is judged on its own.
    let mut left: Vec<String> = Vec::new();
    // Items asked after because their folder went.
    let mut asked: Vec<&str> = Vec::new();
    for (id, base, is_dir) in ITEMS {
        let (place, removed) = match &known.expect[id] {
            Expected::At(place) => (place.as_str(), None),
            Expected::Removed(kind, at) => (base, Some((*kind, at.clone()))),
        };
        let (carriers, recorded, item) = find(id, place, is_dir);
        let mut copies: Vec<(usize, bool)> = Vec::new();
        match item {
            Some(i) => {
                let o = &objects[i];
                let at = if o.names.iter().any(|n| n == place) { place.to_owned() } else { o.names.iter().min().unwrap().clone() };
                out.list.extend(o.names.iter().filter(|n| **n != at && !ignored(n)).map(|n| (n.clone(), "hard-link".to_owned())));
                if o.damaged {
                    // Said in the list; no `update`, whatever its content.
                    out.list.insert((at.clone(), "state-unreadable".to_owned()));
                } else if o.edited {
                    out.rows.push((Update, at.clone(), Some(id.into())));
                } else if at != base {
                    out.rows.push((Move, at.clone(), Some(id.into())));
                }
                next.record.insert(id, o.key);
                next.expect.insert(id, Expected::At(at.clone()));
                out.items.push((id, at));
                copies.extend(carriers.iter().filter(|&&c| c != i).map(|&c| (c, recorded == Some(i))));
            }
            None => {
                copies.extend(carriers.iter().map(|&c| (c, false)));
                let newcomer = (0..objects.len()).find(|&n| !is_dir && removed.is_none() && objects[n].id.is_none() && !objects[n].dir && objects[n].names.iter().any(|n| n == place));
                let looked = seen.named.is_none_or(|named| named(place)) || asked.contains(&id);
                if let Some(n) = newcomer {
                    out.rows.push((Update, place.into(), Some(id.into())));
                    taken.push(n);
                } else if let Some((kind, at)) = removed {
                    if is_dir {
                        left.push(at.clone());
                    }
                    out.rows.push((kind, at, Some(id.into())));
                } else if !looked || under(place, seen.unread) || under(place, &left) {
                } else {
                    let how = missing(id, known, seen.alive);
                    // What is in a folder that went: only `F` in `docs`.
                    let inside = if id == "D" { Some(("F", missing("F", known, seen.alive))) } else { None };
                    let with_it = |to: &str| matches!(&how, Missing::Outside(went) if Path::new(to).starts_with(went));
                    let goes = match (&how, &inside) {
                        (Missing::Gone | Missing::Outside(_), None) => true,
                        (Missing::Gone | Missing::Outside(_), Some((child, within))) => match (find(child, "docs/f.txt", false).2, within) {
                            (Some(_), _) => known.record.contains_key(child),
                            (None, Missing::NoRecord) => false,
                            (None, Missing::Elsewhere(key)) => {
                                out.found.push(*key);
                                false
                            }
                            (None, Missing::Gone) => true,
                            (None, Missing::Outside(to)) => {
                                if !with_it(to) {
                                    out.rows.push((MoveOut, "docs/f.txt".into(), Some((*child).into())));
                                    next.expect.insert(child, Expected::Removed(MoveOut, "docs/f.txt".into()));
                                }
                                true
                            }
                        },
                        (Missing::Elsewhere(key), _) => {
                            out.found.push(*key);
                            false
                        }
                        (Missing::NoRecord, _) => false,
                    };
                    if goes {
                        let kind = if matches!(how, Missing::Gone) { Delete } else { MoveOut };
                        out.rows.push((kind, place.into(), Some(id.into())));
                        next.expect.insert(id, Expected::Removed(kind, place.to_owned()));
                        if is_dir {
                            left.push(place.to_owned());
                        }
                    }
                    if id == "D" {
                        asked.push("F");
                    }
                }
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
            let path = scene.path(name, o.out);
            let may_go = !o.out && want.removed.contains(name);
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
        fx.liveness.alive_tree(&fx.outside);
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
    let named = looked_at(&scene.touched, &scene.objects);
    let named: Option<&dyn Fn(&str) -> bool> = if look == Look::Named { Some(&named) } else { None };
    let listed = if look == Look::Interrupted { listed_around(&after_first_act, &scene.objects) } else { scene.listed(look, &[]) };
    let unread: Vec<String> = listed.iter().filter(|o| o.dir && !scene.objects.iter().any(|now| now.key == o.key)).map(|o| o.names[0].clone()).collect();
    let (first_calls_for, then_known) = outcome(&fx, &Seen { listed: &listed, alive: &scene.objects, unread: &unread, named }, &at_first);
    // An empty copy that look removed: it had listed the item's own recorded object (which
    // the second act may have removed since: seen in that run all the same).
    let went = first_calls_for.removed.clone();
    scene.objects.retain(|o| o.out || !went.contains(&o.names[0]));
    // Whether a look records every item it finds: a batch of names finds only those named.
    let records = handles || look != Look::Named;
    // The outcome: the objects as they are after both acts, with what the first look found
    // by asking, and what the model says the store knows after the first look.
    let listed = scene.listed(look, &first_calls_for.found);
    let (want, _) = outcome(&fx, &Seen { listed: &listed, alive: &scene.objects, unread: &[], named }, &then_known);
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
    assert_eq!(ran, 684);
    assert!(wrong.is_empty(), "{} wrong:\n{}", wrong.len(), wrong.join("\n"));
}
