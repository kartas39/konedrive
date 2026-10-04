use super::*;

/// The words the store keeps, spelled out: a store written by another
/// build reads the same, so a constant reworded fails here.
#[test]
fn the_stored_words_are_spelled_as_they_always_were() {
    assert_eq!(Kind::File.as_str(), "file");
    assert_eq!(Kind::Folder.as_str(), "folder");
    assert_eq!(Placement::Placed.encode(), "placed");
    let skips = [
        (SkipReason::NameTooLong, "skipped:name-too-long"),
        (SkipReason::PersonalVault, "skipped:personal-vault"),
        (SkipReason::Shared, "skipped:shared"),
        (SkipReason::OneNote, "skipped:onenote"),
        (SkipReason::ReservedName, "skipped:reserved-name"),
        (SkipReason::Unsupported, "skipped:unsupported"),
    ];
    for (reason, stored) in skips {
        assert_eq!(Placement::Skipped(reason).encode(), stored);
        assert_eq!(Placement::decode(stored), Placement::Skipped(reason));
    }
    assert_eq!(Placement::decode("placed"), Placement::Placed);
    assert_eq!(Kind::decode("folder"), Kind::Folder);
    assert_eq!(Kind::decode("file"), Kind::File);
}

/// What a damaged store reads as, kept as it is: a placement that is no
/// placement reads as placed, a skip nobody knows as unsupported, a kind
/// that is no kind as a file.
#[test]
fn an_unknown_word_reads_as_it_always_did() {
    assert_eq!(Placement::decode("nonsense"), Placement::Placed);
    assert_eq!(Placement::decode(""), Placement::Placed);
    assert_eq!(Placement::decode("skipped:nonsense"), Placement::Skipped(SkipReason::Unsupported));
    assert_eq!(Kind::decode("nonsense"), Kind::File);
}

#[test]
fn a_column_is_found_by_its_name_in_the_list() {
    assert_eq!(column(ROW_COLUMNS, "id"), 0);
    assert_eq!(column(ROW_COLUMNS, "name"), 2);
    assert_eq!(column(ROW_COLUMNS, "placement"), 10);
    assert_eq!(ROW_WIDTH, 11);
    // A name that begins or ends another is not it; a list broken over lines reads the same.
    assert_eq!(column("parent_id, id,\n     name", "id"), 1);
    assert_eq!(column("parent_id, id,\n     name", "name"), 2);
    assert_eq!(width("a,b , c"), 3);
    assert_eq!(width(""), 0);
    assert!(COLUMNS.starts_with(ROW_COLUMNS), "a copied row begins with the columns a row is read by");
}

/// Every row comes back as it went in, through the one decoder: from
/// `items`, from `staging`, and from `deferred`.
#[test]
fn a_row_reads_back_as_written_from_every_table() {
    let row = |id: &str, kind, placement| Row {
        id: id.into(),
        parent_id: Some("R".into()),
        name: format!("n-{id}"),
        kind,
        size: 7,
        mtime: -3,
        etag: Some("e".into()),
        ctag: Some("c".into()),
        quickxor: Some("q".into()),
        mime: Some("text/plain".into()),
        placement,
    };
    let root = Row { id: "R".into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed };
    let file = row("F", Kind::File, Placement::Placed);
    let skipped = row("S", Kind::Folder, Placement::Skipped(SkipReason::Shared));
    let mut store = crate::TreeStore::in_memory().unwrap();
    store.begin_staging(false).unwrap();
    store.stage(&[Change::Root(root), Change::Upsert(file.clone()), Change::Upsert(skipped.clone())]).unwrap();
    assert_eq!(store.get(Table::Staging, "F").unwrap(), Some(file.clone()));
    store.commit_staging("link").unwrap();
    assert_eq!(store.get(Table::Items, "F").unwrap(), Some(file.clone()));
    assert_eq!(store.get(Table::Items, "S").unwrap(), Some(skipped.clone()));
    assert_eq!(store.children(Table::Items, "R").unwrap(), vec![file.clone(), skipped]);

    let changed = Row { ctag: Some("c2".into()), size: 9, ..file };
    store.begin_staging(true).unwrap();
    store.stage(&[Change::Upsert(changed.clone()), Change::Delete("S".into())]).unwrap();
    store.commit_staging_deferring("link-2", &[], &["F".into(), "S".into()], &[], 1).unwrap();
    assert_eq!(store.deferred("F").unwrap(), Some(Change::Upsert(changed)));
    assert_eq!(store.deferred("S").unwrap(), Some(Change::Delete("S".into())));
}
