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
    assert!(plan.of("not asked").base.is_none() && plan.of("not asked").new.is_none());
}
