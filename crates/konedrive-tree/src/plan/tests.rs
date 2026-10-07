use std::path::Path;

use crate::model::{Change, Kind, Placement, Row, SkipReason};
use crate::{NewTree, TreeStore};

fn row(id: &str, parent: &str, name: &str, kind: Kind) -> Row {
    Row { id: id.into(), parent_id: Some(parent.into()), name: name.into(), kind, size: 1, mtime: 0, etag: None, ctag: Some(format!("c-{id}")), quickxor: None, mime: None, placement: Placement::Placed }
}

fn root() -> Change {
    Change::Root(Row { id: "R".into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed })
}

/// One call says, for every item asked, what the base has and where, and
/// what the new tree has and where: an item renamed, one removed, one new
/// inside a folder that comes into view, one no longer placed, and an id
/// neither tree has.
#[test]
fn the_plan_gives_each_items_base_and_new_side() {
    let mut s = TreeStore::in_memory().unwrap();
    s.begin_staging(NewTree::Whole).unwrap();
    s.stage(&[
        root(),
        Change::Upsert(row("D", "R", "docs", Kind::Folder)),
        Change::Upsert(row("F", "D", "f.txt", Kind::File)),
        Change::Upsert(row("G", "R", "g.txt", Kind::File)),
        Change::Upsert(Row { placement: Placement::Skipped(SkipReason::Shared), ..row("S", "R", "shared", Kind::Folder) }),
        Change::Upsert(row("V", "R", "vault", Kind::Folder)),
    ])
    .unwrap();
    s.commit_staging("L1").unwrap();
    s.begin_staging(NewTree::Delta).unwrap();
    s.stage(&[
        Change::Upsert(row("F", "D", "renamed.txt", Kind::File)),
        Change::Delete("G".into()),
        Change::Upsert(row("S", "R", "shared", Kind::Folder)),
        Change::Upsert(row("N", "S", "n.txt", Kind::File)),
        Change::Upsert(Row { placement: Placement::Skipped(SkipReason::PersonalVault), ..row("V", "R", "vault", Kind::Folder) }),
    ])
    .unwrap();

    let ids: Vec<String> = ["F", "G", "S", "N", "V", "D", "nobody's", "F"].iter().map(|id| id.to_string()).collect();
    let plan = s.plan(&ids).unwrap();
    assert_eq!(plan.ids().count(), 7, "an id asked twice is planned once");

    let renamed = plan.of("F");
    assert_eq!(renamed.base_place().unwrap().rel, Path::new("docs/f.txt"));
    assert_eq!(renamed.new_place().unwrap().rel, Path::new("docs/renamed.txt"));
    assert_eq!(renamed.new.as_ref().unwrap().row.name, "renamed.txt");
    assert!(!renamed.stays() && !renamed.comes_into_view());

    let removed = plan.of("G");
    assert_eq!(removed.base_place().unwrap().depth, 1);
    assert!(removed.new.is_none());

    let shown = plan.of("S");
    assert!(shown.comes_into_view(), "skipped by the base, placed by the new tree");
    assert!(shown.base.as_ref().is_some_and(|base| !base.placed()));

    let new = plan.of("N");
    assert!(new.base.is_none());
    assert_eq!(new.new_place().unwrap().rel, Path::new("shared/n.txt"));

    let unplaced = plan.of("V");
    assert!(unplaced.base_place().is_some());
    assert!(unplaced.new.is_some() && unplaced.new_place().is_none(), "the new tree has it and does not place it");

    let untouched = plan.of("D");
    assert!(untouched.stays(), "the new tree of a delta is the base where the delta says nothing");

    assert!(plan.of("nobody's").base.is_none() && plan.of("nobody's").new.is_none());
    assert!(plan.has("nobody's") && !plan.has("not asked"));

    let rows = s.new_rows(&ids).unwrap();
    assert_eq!(rows.len(), 5, "the rows the new tree has: not the removed one, not the unknown one");
    assert_eq!(rows["F"].name, "renamed.txt");
    assert_eq!(rows["D"].name, "docs", "the base's row, where the delta says nothing");
}

/// The root is planned as placed at the folder itself; an item whose
/// folder never arrived has a row and no place; and after a whole listing
/// the new tree is the listing alone, so what it leaves out has no new side.
#[test]
fn the_plan_of_the_root_of_an_orphan_and_of_a_whole_listing() {
    let mut s = TreeStore::in_memory().unwrap();
    s.begin_staging(NewTree::Whole).unwrap();
    s.stage(&[root(), Change::Upsert(row("D", "R", "docs", Kind::Folder)), Change::Upsert(row("F", "D", "f.txt", Kind::File)), Change::Upsert(row("O", "missing", "o.txt", Kind::File))]).unwrap();
    s.commit_staging("L1").unwrap();
    s.begin_staging(NewTree::Whole).unwrap();
    s.stage(&[root(), Change::Upsert(row("D", "R", "papers", Kind::Folder)), Change::Upsert(row("O", "missing", "o.txt", Kind::File))]).unwrap();

    let ids: Vec<String> = ["R", "D", "F", "O"].iter().map(|id| id.to_string()).collect();
    let plan = s.plan(&ids).unwrap();

    let top = plan.of("R");
    assert_eq!(top.base_place().unwrap().depth, 0);
    assert_eq!(top.new_place().unwrap().rel, Path::new(""));

    let orphan = plan.of("O");
    assert!(orphan.base.as_ref().is_some_and(|base| base.at.is_none() && !base.placed()), "a row, and nowhere");
    assert!(orphan.new.as_ref().is_some_and(|new| new.at.is_none()));
    assert!(!orphan.comes_into_view());

    assert_eq!(plan.of("D").new_place().unwrap().rel, Path::new("papers"));
    let left_out = plan.of("F");
    assert_eq!(left_out.base_place().unwrap().rel, Path::new("docs/f.txt"));
    assert!(left_out.new.is_none(), "a whole listing that leaves it out does not have it");
    assert!(!s.new_rows(&ids).unwrap().contains_key("F"));
}
