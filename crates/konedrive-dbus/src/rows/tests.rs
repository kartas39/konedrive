use zbus::zvariant::{OwnedValue, Type, Value};

use super::*;

/// What a reply with `body` reads as: a row type is read from what the daemon sends, which
/// is a tuple of the same fields.
fn read<T>(body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> T
where
    T: for<'d> serde::Deserialize<'d> + Type,
{
    let message = zbus::message::Message::method_call("/org/konedrive/Accounts", "Ping").unwrap().build(body).unwrap();
    message.body().deserialize().unwrap()
}

fn s(text: &str) -> String {
    text.to_owned()
}

/// Each row is the structure its interface's XML gives, and the fields are read in its order.
#[test]
fn a_row_is_read_from_the_structure_the_xml_gives() {
    let xml = |interface: &str| {
        let path = format!("{}/../../dbus/org.konedrive.{interface}.xml", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    };
    let array_of = |signature: &zbus::zvariant::Signature| format!("type=\"a{signature}\"");
    assert!(xml("UploadQueue").contains(&array_of(Change::SIGNATURE)), "{}", Change::SIGNATURE);
    assert!(xml("UploadQueue").contains(&array_of(KeptBackReason::SIGNATURE)), "{}", KeptBackReason::SIGNATURE);
    assert!(xml("UploadQueue").contains(&array_of(KeptBack::SIGNATURE)), "{}", KeptBack::SIGNATURE);
    assert!(xml("Conflicts").contains(&array_of(Conflict::SIGNATURE)), "{}", Conflict::SIGNATURE);
    assert!(xml("ActivityLog").contains(&array_of(Event::SIGNATURE)), "{}", Event::SIGNATURE);
    assert!(xml("Transfers").contains(&array_of(Transfer::SIGNATURE)), "{}", Transfer::SIGNATURE);

    let changes: Vec<Change> = read(&(vec![(7u64, "create", "/f/a", "retry", 1u64, 2u64, "network", 99i64)],));
    let change = Change { seq: 7, kind: s("create"), path: s("/f/a"), state: s("retry"), sent: 1, total: 2, reason: s("network"), next_try: 99 };
    assert_eq!(changes, [change]);
    let reasons: Vec<KeptBackReason> = read(&(vec![("per-file", "refused", 3u32, 4u64)],));
    assert_eq!(reasons, [KeptBackReason { group: s("per-file"), reason: s("refused"), count: 3, bytes: 4 }]);
    let kept: Vec<KeptBack> = read(&(vec![("/f/a", "symlink")],));
    assert_eq!(kept, [KeptBack { path: s("/f/a"), reason: s("symlink") }]);
    let files: KeptBackFiles = read(&(vec![("/f/a", "refused: bad name")], 25u32));
    assert_eq!(files, KeptBackFiles { items: vec![KeptBack { path: s("/f/a"), reason: s("refused: bad name") }], total: 25 });
    let conflicts: Vec<Conflict> = read(&(vec![(5i64, "/f/a", "/f/a-copy", "copy")],));
    assert_eq!(conflicts, [Conflict { at: 5, original: s("/f/a"), kept: s("/f/a-copy"), how: s("copy") }]);
    assert!(conflicts[0].is_copy());
    let events: Vec<Event> = read(&(vec![(5i64, "downloaded", "/f/a", "")],));
    assert_eq!(events, [Event { at: 5, kind: s("downloaded"), path: s("/f/a"), detail: s("") }]);
    assert_eq!(read::<Freed>(&(1u32, 2u64, 3u32, 4u32)), Freed { files: 1, bytes: 2, busy: 3, pinned: 4 });
    assert_eq!(read::<FreedSpace>(&(1u32, 2u64, 3u32)), FreedSpace { files: 1, bytes: 2, busy: 3 });

    // A property comes as a value.
    let value = OwnedValue::try_from(Value::from(vec![("/f/a", 1u64, 2u64)])).unwrap();
    let transfers = Vec::<Transfer>::try_from(value).unwrap();
    assert_eq!(transfers, [Transfer { path: s("/f/a"), done: 1, total: 2 }]);
}
