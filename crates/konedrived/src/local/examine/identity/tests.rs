//! The identity rule as a table: each state an object can be in
//! (`docs/tasks/object-states.md` section 2) and what the examination decides of it,
//! with no disk and no store.

use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{Stamp, State};
use konedrive_tree::outbox::LocalSkip;
use konedrive_tree::Kind;

use super::*;
use crate::local::entry::StateAttr;
use crate::local::examine::copies::{fate, Fate};
use crate::local::examine::found::{verdict, Verdict};
use crate::local::examine::listing::EntryIx;

fn handle(object: u64) -> FileHandle {
    let mut stored = 1i32.to_le_bytes().to_vec();
    stored.extend_from_slice(&object.to_le_bytes());
    FileHandle::decode(&stored).unwrap()
}

/// A downloaded file at `rel`, the object `object`, carrying `id`, as the base's
/// version left it.
fn entry(rel: &str, object: u64, id: Option<&str>) -> Entry {
    Entry {
        rel: PathBuf::from(rel),
        name: Path::new(rel).file_name().unwrap().to_owned(),
        ty: Type::File,
        dev: 1,
        ino: object,
        nlink: 1,
        size: 3,
        mtime: (10, 0),
        id: id.map(str::to_owned),
        state: if id.is_some() { StateAttr::Known(State::Hydrated) } else { StateAttr::Absent },
        stamp: id.map(|_| Stamp { size: 3, mtime_sec: 10, mtime_nsec: 0 }),
        ctag: None,
        handle: Some(handle(object)),
    }
}

fn with_state(mut e: Entry, state: StateAttr) -> Entry {
    e.state = state;
    e
}

fn seen(entries: &[Entry]) -> Vec<Seen<'_>> {
    entries.iter().enumerate().map(|(n, entry)| Seen { ix: EntryIx::nth(n), entry, ignored: entry.rel.to_string_lossy().ends_with('~'), pending: false, creating: false }).collect()
}

fn ix(n: usize) -> EntryIx {
    EntryIx::nth(n)
}

/// The item `A`, a file placed at `a.txt`, its object 7 on record.
fn placed<'a>(recorded: Option<&'a FileHandle>) -> Known<'a> {
    Known::Placed { kind: Kind::File, recorded, expected: Some(Path::new("a.txt")) }
}

/// States B, C, D, E and X: the recorded object is the item, whatever its content's
/// state; what that state means is the content check's to say, before any open.
#[test]
fn the_recorded_object_is_the_item_and_its_marks_say_what_is_read() {
    let recorded = handle(7);
    let states = [
        ("B", StateAttr::Known(State::OnlineOnly), Verdict::NotDownloaded { cut: false }),
        ("C", StateAttr::Known(State::Hydrated), Verdict::Same),
        ("E", StateAttr::Known(State::Hydrating), Verdict::InTransit),
        ("E", StateAttr::Known(State::Dehydrating), Verdict::InTransit),
        ("X", StateAttr::Absent, Verdict::Damaged),
        ("X", StateAttr::Corrupt, Verdict::Damaged),
    ];
    for (state, marks, read) in states {
        // Wherever it stands: at its place, or moved.
        for rel in ["a.txt", "docs/b.txt"] {
            let entries = [with_state(entry(rel, 7, Some("A")), marks)];
            let who = identify("A", placed(Some(&recorded)), &seen(&entries), None);
            assert_eq!(who, Identity { item: Some(ix(0)), certain: true, ..Identity::default() }, "{state} at {rel}");
            assert_eq!(verdict(&entries[0], 3, false, false), read, "{state}");
        }
    }
    // D: another size, another time, or a write seen, and the file is read.
    let mut edited = entry("a.txt", 7, Some("A"));
    edited.size = 9;
    assert_eq!(verdict(&edited, 3, false, false), Verdict::ReadIt { size_changed: true });
    let mut touched = entry("a.txt", 7, Some("A"));
    touched.mtime = (11, 0);
    assert_eq!(verdict(&touched, 3, false, false), Verdict::ReadIt { size_changed: false });
    assert_eq!(verdict(&entry("a.txt", 7, Some("A")), 3, false, true), Verdict::ReadIt { size_changed: false });
    // Not while the worker is sending it as it is.
    assert_eq!(verdict(&edited, 3, true, false), Verdict::BeingSent);
    // B cut by `truncate`.
    let cut = with_state(edited, StateAttr::Known(State::OnlineOnly));
    assert_eq!(verdict(&cut, 3, false, false), Verdict::NotDownloaded { cut: true });
}

/// States F and G: another object with the id is a copy, wherever the item's own object
/// is; certainly not the item only when the recorded object was seen. With content its
/// marks come off; empty, it is listed, or removed when certain.
#[test]
fn another_object_with_the_id_is_a_copy() {
    let recorded = handle(7);
    let f = entry("copy.txt", 8, Some("A"));
    let g = with_state(entry("copy.bin", 9, Some("A")), StateAttr::Known(State::OnlineOnly));
    assert_eq!(fate(&f), Fate::Stripped);
    assert_eq!(fate(&g), Fate::NotDownloaded);
    // The original seen: at its place, or moved.
    for rel in ["a.txt", "docs/a.txt"] {
        let entries = [entry(rel, 7, Some("A")), f.clone(), g.clone()];
        let who = identify("A", placed(Some(&recorded)), &seen(&entries), None);
        assert_eq!(who, Identity { item: Some(ix(0)), copies: vec![ix(1), ix(2)], certain: true, ..Identity::default() }, "{rel}");
    }
    // The original not seen (gone, moved out, or where this run did not look): still copies,
    // and nothing is the item. Not certain: an empty one stays.
    let entries = [f.clone(), g.clone()];
    let who = identify("A", placed(Some(&recorded)), &seen(&entries), None);
    assert_eq!(who, Identity { copies: vec![ix(0), ix(1)], ..Identity::default() });
    // No record (a rebuilt store): the object at the item's place is the item, the rest
    // are copies, and nothing is certain.
    let entries = [f.clone(), entry("a.txt", 7, Some("A"))];
    let who = identify("A", placed(None), &seen(&entries), None);
    assert_eq!(who, Identity { item: Some(ix(1)), copies: vec![ix(0)], ..Identity::default() });
    // A copy standing at the item's place while the recorded object is elsewhere and
    // seen: the recorded one is the item.
    let entries = [entry("a.txt", 8, Some("A")), entry("b.txt", 7, Some("A"))];
    let who = identify("A", placed(Some(&recorded)), &seen(&entries), None);
    assert_eq!(who, Identity { item: Some(ix(1)), copies: vec![ix(0)], certain: true, ..Identity::default() });
    // A directory with a file's id, and an item the base does not place: copies.
    let mut dir = entry("a.txt", 7, Some("A"));
    dir.ty = Type::Dir;
    let who = identify("A", placed(Some(&recorded)), &seen(std::slice::from_ref(&dir)), None);
    assert_eq!(who, Identity { copies: vec![ix(0)], ..Identity::default() });
    assert_eq!(fate(&dir), Fate::Stripped);
    let entries = [entry("a.txt", 7, Some("A"))];
    let who = identify("A", Known::Unplaced { kind: Kind::File, placing: None }, &seen(&entries), None);
    assert_eq!(who, Identity { copies: vec![ix(0)], ..Identity::default() });
    // ... unless a reconcile is placing it right there: it waits.
    let who = identify("A", Known::Unplaced { kind: Kind::File, placing: Some(Path::new("a.txt")) }, &seen(&entries), None);
    assert_eq!(who, Identity { waits: vec![ix(0)], ..Identity::default() });
}

/// State H: an id the store does not know. A copy, never certain; left alone while a
/// reconcile is placing the id, or while its own create is between its two commits.
#[test]
fn an_unknown_id_is_a_copy_unless_it_is_on_its_way_in() {
    let entries = [entry("h.txt", 7, Some("H")), with_state(entry("h.bin", 8, Some("H")), StateAttr::Known(State::OnlineOnly))];
    let who = identify("H", Known::Unknown { placing: false }, &seen(&entries), None);
    assert_eq!(who, Identity { copies: vec![ix(0), ix(1)], ..Identity::default() });
    let who = identify("H", Known::Unknown { placing: true }, &seen(&entries), None);
    assert_eq!(who, Identity { waits: vec![ix(0), ix(1)], ..Identity::default() });
    let mut pending = seen(&entries);
    pending[0].pending = true;
    let who = identify("H", Known::Unknown { placing: false }, &pending, None);
    assert_eq!(who, Identity { copies: vec![ix(1)], ..Identity::default() });
}

/// State K: other names of the item's object are links; the item is the name at its
/// place. A copy with content and a second name is listed, not stripped.
#[test]
fn a_second_name_of_the_items_object_is_a_link() {
    let recorded = handle(7);
    let mut entries = [entry("z.txt", 7, Some("A")), entry("a.txt", 7, Some("A")), entry("docs/k.txt", 7, Some("A"))];
    entries.iter_mut().for_each(|e| e.nlink = 3);
    let who = identify("A", placed(Some(&recorded)), &seen(&entries), None);
    assert_eq!(who, Identity { item: Some(ix(1)), links: vec![ix(2), ix(0)], certain: true, ..Identity::default() });
    assert_eq!(fate(&entries[0]), Fate::HardLink);
}

/// The editors' save with a backup: the recorded object under an ignored name beside
/// the item's place, and another file there, new or with the attributes copied.
#[test]
fn the_recorded_object_under_a_backup_name_gives_the_item_to_the_file_at_its_place() {
    let recorded = handle(7);
    for id in [None, Some("A")] {
        let new = entry("a.txt", 8, id);
        let entries = [entry("a.txt~", 7, Some("A")), new];
        let carriers: Vec<Seen<'_>> = seen(&entries).into_iter().filter(|s| s.entry.id.is_some()).collect();
        let who = identify("A", placed(Some(&recorded)), &carriers, Some(seen(&entries)[1]));
        assert_eq!(who, Identity { backup: Some(Backup { old: vec![ix(0)], new: ix(1) }), ..Identity::default() }, "{id:?}");
    }
    // Not for a file the worker is creating right now: the recorded object stays the item.
    let entries = [entry("a.txt~", 7, Some("A")), entry("a.txt", 8, None)];
    let mut at_place = seen(&entries)[1];
    at_place.creating = true;
    let who = identify("A", placed(Some(&recorded)), &seen(&entries)[..1], Some(at_place));
    assert_eq!(who, Identity { item: Some(ix(0)), certain: true, ..Identity::default() });
}

/// State A, and what is never an object of OneDrive: an entry without an id is not asked
/// about at all, it is new; a link or a reserved name is listed.
#[test]
fn what_carries_no_id_is_new_and_what_onedrive_cannot_hold_is_listed() {
    use crate::local::examine::sort;
    use crate::local::examine::listing::Listing;
    let mut link = entry("link", 3, None);
    link.ty = Type::Symlink;
    let mut elsewhere = entry("mount", 4, None);
    elsewhere.dev = 2;
    let listing = Listing::of(vec![entry("new.txt", 1, None), entry("a.txt", 7, Some("A")), link, elsewhere, entry(".konedrive-mine", 5, None)]);
    let sorted = sort(&listing, &crate::local::IgnoreList::default(), 1);
    assert_eq!(sorted.unnamed, vec![ix(0)]);
    assert_eq!(sorted.by_id.into_iter().collect::<Vec<_>>(), vec![("A".to_owned(), vec![ix(1)])]);
    assert_eq!(sorted.listed, vec![(ix(2), LocalSkip::Symlink), (ix(3), LocalSkip::OtherDevice), (ix(4), LocalSkip::ReservedName)]);
}
