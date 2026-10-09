//! Names exchanged in the folder: a row whose name in OneDrive is still held by an item
//! another row is taking away goes through a temporary name (`.konedrive-swap-*`, §5.3).

use super::*;
use konedrive_tree::outbox::Detection;

/// A swap, and a folder replaced by its own subfolder, end to end: each goes through
/// a temporary name, nothing is adopted or copied, and nothing but what the
/// user deleted is deleted.
#[test]
fn swaps_and_folders_replaced_in_place_go_through_a_temporary_name() {
    // The subfolder: mv F/sub F.tmp && rm -rf F && mv F.tmp F.
    let w = World::new(&[folder("F", "R", "F"), folder("S", "F", "sub"), file("A", "S", "a.txt", b"a"), file("B", "F", "b.txt", b"b")]);
    w.rename_entry("F/sub", "F.tmp");
    w.remove_entry("F");
    w.rename_entry("F.tmp", "F");
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["F", "F/a.txt"]);
    assert_eq!(w.id_at("F").as_deref(), Some("S"));
    assert!(w.cloud(|c| c.bin.contains_key("F") && c.bin.contains_key("B") && !c.bin.contains_key("A")));
    assert!(w.cloud(|c| c.count("PATCH", "items/S")) >= 2, "through the temporary name");

    // A swap: a and b exchanged.
    let w = World::new(&[file("A", "R", "a", b"a"), file("B", "R", "b", b"b")]);
    w.rename_entry("a", "tmp");
    w.rename_entry("b", "a");
    w.rename_entry("tmp", "b");
    assert_eq!(w.rows().len(), 2, "{:?}", w.summary());
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!((w.id_at("a").as_deref(), w.id_at("b").as_deref()), (Some("B"), Some("A")));
    assert!(w.cloud(|c| c.bin.is_empty() && c.paths().iter().all(|p| !p.contains(SWAP_PREFIX))));
}

/// A swap where one side was also edited. The edited file goes through
/// a temporary name; its content is throttled after the PATCH to that name
/// landed, and the replay meets `412`. The file keeps its name here, and
/// OneDrive ends with both files swapped and nothing under a temporary name.
/// Then the same for a move whose PATCH answer was lost and whose row an
/// examination merged into before the replay: it keeps its temporary name.
#[test]
fn a_row_through_a_temporary_name_keeps_the_users_name_after_a_retry() {
    for edited in [true, false] {
        let w = World::new(&[file("A", "R", "a", b"a"), file("B", "R", "b", b"b")]);
        w.download("a", b"a");
        w.download("b", b"b");
        w.rename_entry("a", "tmp");
        w.rename_entry("b", "a");
        w.rename_entry("tmp", "b");
        if edited {
            w.write_file("b", b"a, edited");
        }
        let of = |id: &str| w.rows().into_iter().find(|r| r.item_id.as_deref() == Some(id)).unwrap();
        // B's move is held back, so that A's row meets b still taken.
        let b_seq = of("B").seq;
        w.store.call_blocking(move |s| s.outbox_set_state(b_seq, OutboxState::Held, None, None)).unwrap();
        let engine = w.h.engine();
        if edited {
            w.cloud(|c| c.script("POST", "createUploadSession", ResponseTemplate::new(429).insert_header("Retry-After", "1"), 1));
        } else {
            engine.arm(Fault::AfterSend);
        }
        w.h.drain(&engine);
        let a = of("A");
        assert!(a.target_name.as_deref().is_some_and(|n| n.starts_with(SWAP_PREFIX)), "{edited}: {a:?}");
        assert!(w.cloud(|c| c.item("A").unwrap().name.starts_with(SWAP_PREFIX)), "the PATCH to the temporary name landed");
        if !edited {
            // Backed off, then said to be where it is once more: the merge keeps the temporary name.
            w.store.call_blocking(move |s| s.outbox_set_state(a.seq, OutboxState::Retry, Some(&"test".into()), Some(0))).unwrap();
            let again = Detection { item_id: Some("A".into()), base: a.base.clone(), same_content: true, ..w.detection(Move, Path::new("b"), a.object.clone(), None) };
            w.record(vec![again], Vec::new());
            assert_eq!(of("A").target_name, a.target_name, "a replay looks for it there");
        }
        w.store.call_blocking(move |s| s.outbox_set_state(b_seq, OutboxState::Ready, None, None)).unwrap();
        w.run();
        assert!(w.rows().is_empty(), "{edited}: {:?}", w.summary());
        let here: Vec<String> = std::fs::read_dir(&w.root.path).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(here.iter().all(|n| !n.starts_with(SWAP_PREFIX)), "{edited}: {here:?}");
        assert_eq!((w.id_at("a").as_deref(), w.id_at("b").as_deref()), (Some("B"), Some("A")), "{edited}");
        assert_eq!((w.attr("a", XATTR_ITEM_ID).as_deref(), w.attr("b", XATTR_ITEM_ID).as_deref()), (Some("B"), Some("A")));
        assert!(w.cloud(|c| c.bin.is_empty() && c.paths().iter().all(|p| !p.contains(SWAP_PREFIX))), "{edited}");
        if edited {
            assert_eq!(w.content("b").unwrap(), b"a, edited");
            assert_committed(&w, "b", "b");
        }
    }
}

/// A folder wrapped in a new one of its name (`mkdir t && mv d t/ && mv t d`): the new
/// folder goes through a temporary name, the old one moves into it, and nothing is deleted.
#[test]
#[ignore = "the rows wait for each other for ever: the new folder is in the base under its temporary name, so `steps::shared::dir_id` (which asks where the base places the id, no longer which directory it is) does not give its id to the move of the old folder into it, and the new folder's move to its name waits for that move to free the name"]
fn a_folder_wrapped_in_a_new_one_of_its_name_goes_through_a_temporary_name() {
    let w = World::new(&[folder("D", "R", "d"), file("X", "D", "x.txt", b"x")]);
    w.make_directory("t");
    w.rename_entry("d", "t/d");
    w.rename_entry("t", "d");
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["d", "d/d", "d/d/x.txt"]);
    assert_eq!(w.id_at("d/d").as_deref(), Some("D"));
    assert!(w.cloud(|c| c.bin.is_empty()));
    assert_committed(&w, "d", "d");
}
