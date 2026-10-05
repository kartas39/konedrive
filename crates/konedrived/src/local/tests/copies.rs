//! A small enumeration of what the user can do with an item's object and
//! its marks, in an examination of the names concerned and in one of the
//! whole folder: a copy that kept the marks (`cp -a`, or a snapshot brought
//! back) of a downloaded file, of a file not downloaded and of a folder; a
//! second name (`ln`); a save
//! by rename whose new file copied the marks. In every one the item stays
//! the item, a copy with content gets one row as a new object, an empty
//! copy is removed because the item's own object was seen in the same run,
//! and no row deletes, renames or moves anything in OneDrive.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    CopyFile,
    CopyPlaceholder,
    CopyFolder,
    LinkFile,
    LinkPlaceholder,
    SaveByRename,
}

/// A copy of `from` with every attribute, as a snapshot or a backup restores it: a file
/// that is not downloaded is copied as it is, with no content, since reading it (as `cp`
/// does) would download it first.
pub(super) fn copy_as_it_is(from: &Path, to: &Path) {
    if xattr::get(from, placeholder::XATTR_STATE).unwrap().as_deref() != Some(b"online-only") {
        return copy_keeping_attributes(from, to);
    }
    File::create(to).unwrap().set_len(std::fs::metadata(from).unwrap().len()).unwrap();
    for name in xattr::list(from).unwrap() {
        xattr::set(to, &name, &xattr::get(from, &name).unwrap().unwrap()).unwrap();
    }
}

/// The same for a directory: itself and what is in it.
pub(super) fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir(to).unwrap();
    for name in xattr::list(from).unwrap() {
        xattr::set(to, &name, &xattr::get(from, &name).unwrap().unwrap()).unwrap();
    }
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to.join(entry.file_name()));
        } else {
            copy_as_it_is(&entry.path(), &to.join(entry.file_name()));
        }
    }
}

/// (directory, name) of a batch, or (place, reason) of the skipped list.
type Pairs = Vec<(&'static str, &'static str)>;
/// (kind, place, item) of the rows expected.
type Rows = Vec<(OutboxKind, &'static str, Option<&'static str>)>;

/// One combination: what is wrong with it, if anything.
fn run(act: Act, full: bool) -> Vec<String> {
    let fx = Folder::new(&[folder("D", "R", "docs"), file("A", "R", "a.txt", b"abc"), file("P", "R", "p.bin", b"only in the cloud"), file("F", "D", "f.txt", b"ff"), file("Q", "D", "q.bin", b"cloud")]);
    fx.hydrate("a.txt", b"abc");
    fx.hydrate("docs/f.txt", b"ff");
    let items = [("a.txt", "A"), ("p.bin", "P"), ("docs", "D"), ("docs/f.txt", "F"), ("docs/q.bin", "Q")];
    let recorded: Vec<FileHandle> = items.iter().map(|(rel, _)| fx.handle(rel)).collect();
    // The names the user's act touched, and the rows and the list it must leave.
    let (named, rows, list): (Pairs, Rows, Pairs) = match act {
        Act::CopyFile => {
            copy_keeping_attributes(&fx.path("a.txt"), &fx.path("a2.txt"));
            (vec![("", "a2.txt")], vec![(Create, "a2.txt", None)], vec![])
        }
        Act::CopyPlaceholder => {
            copy_as_it_is(&fx.path("p.bin"), &fx.path("p2.bin"));
            (vec![("", "p2.bin")], vec![], vec![])
        }
        Act::CopyFolder => {
            copy_tree(&fx.path("docs"), &fx.path("docs2"));
            (vec![("", "docs2")], vec![(Mkdir, "docs2", None), (Create, "docs2/f.txt", None)], vec![])
        }
        Act::LinkFile => {
            std::fs::hard_link(fx.path("a.txt"), fx.path("a2.txt")).unwrap();
            (vec![("", "a2.txt")], vec![], vec![("a2.txt", "hard-link")])
        }
        Act::LinkPlaceholder => {
            std::fs::hard_link(fx.path("p.bin"), fx.path("p2.bin")).unwrap();
            (vec![("", "p2.bin")], vec![], vec![("p2.bin", "hard-link")])
        }
        Act::SaveByRename => {
            fx.write("a.txt.new", b"the new text");
            for name in xattr::list(fx.path("a.txt")).unwrap() {
                xattr::set(fx.path("a.txt.new"), &name, &xattr::get(fx.path("a.txt"), &name).unwrap().unwrap()).unwrap();
            }
            fx.rename("a.txt.new", "a.txt");
            (vec![("", "a.txt"), ("", "a.txt.new")], vec![(Update, "a.txt", Some("A"))], vec![])
        }
    };
    let batch = if full { Batch::full() } else { names(&named) };
    let mut wrong = Vec::new();
    fx.examine(&batch);
    let rows: Vec<(OutboxKind, String, Option<String>)> = rows.into_iter().map(|(kind, rel, id)| (kind, rel.to_owned(), id.map(str::to_owned))).collect();
    let list: Vec<(String, String)> = list.into_iter().map(|(rel, why)| (rel.to_owned(), why.to_owned())).collect();
    let first = (fx.summary(), fx.skipped());
    if first.0 != rows {
        wrong.push(format!("the rows are {:?}", first.0));
    }
    if first.1 != list {
        wrong.push(format!("the list is {:?}", first.1));
    }
    // Looked at again, the same: what went up as new goes up once.
    fx.examine(&batch);
    if (fx.summary(), fx.skipped()) != first {
        wrong.push(format!("a second look changes it: {:?}, {:?}", fx.summary(), fx.skipped()));
    }
    // The item stays the item: its object keeps its marks, and the base's record of it.
    for ((rel, id), was) in items.iter().zip(&recorded) {
        if act == Act::SaveByRename && *id == "A" {
            continue;
        }
        if id_of(&fx.path(rel)).as_deref() != Some(*id) {
            wrong.push(format!("{rel} no longer carries {id}"));
        }
        if fx.handle(rel) != *was || fx.store.call_blocking({ let id = id.to_string(); move |s| s.local_handle(&id) }).unwrap().as_ref() != Some(was) {
            wrong.push(format!("the object of {id} is another one"));
        }
    }
    if act == Act::SaveByRename && id_of(&fx.path("a.txt")).as_deref() != Some("A") {
        wrong.push("the saved file is not the item".into());
    }
    // A copy with content is the user's own file, with its content. An empty copy goes,
    // and that is said: the run looks at the item's own place too, and sees its object
    // there (the copy that stays because the item's object was not seen is in
    // `identity.rs`).
    for (copy, content) in [("a2.txt", &b"abc"[..]), ("docs2/f.txt", b"ff")] {
        if fx.path(copy).exists() && std::fs::read(fx.path(copy)).unwrap() != content {
            wrong.push(format!("{copy} lost its content"));
        }
        if matches!(act, Act::CopyFile | Act::CopyFolder) && fx.path(copy).exists() && id_of(&fx.path(copy)).is_some() {
            wrong.push(format!("{copy} still carries an item's id"));
        }
    }
    let said = fx.activity().len();
    for (empty, by) in [("p2.bin", Act::CopyPlaceholder), ("docs2/q.bin", Act::CopyFolder)] {
        if act == by && (fx.path(empty).exists() || said != 1) {
            wrong.push(format!("{empty} is there: {}, and Activity has {said} line(s)", fx.path(empty).exists()));
        }
    }
    wrong
}

#[test]
fn what_the_user_does_with_an_items_marks_never_changes_the_item() {
    let mut wrong = Vec::new();
    for act in [Act::CopyFile, Act::CopyPlaceholder, Act::CopyFolder, Act::LinkFile, Act::LinkPlaceholder, Act::SaveByRename] {
        for full in [false, true] {
            wrong.extend(run(act, full).into_iter().map(|what| format!("{act:?}, full={full}: {what}")));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
