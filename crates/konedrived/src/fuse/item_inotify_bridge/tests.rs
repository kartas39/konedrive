use std::sync::mpsc;

use konedrive_tree::{ItemPlace, LocalItemId};

use super::*;

fn appeared(number: i64) -> ItemAnnouncement {
    ItemAnnouncement::Appeared { item: LocalItemId::new(number), place: ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: format!("item-{number}").into() } }
}

fn bridge() -> Arc<ItemInotifyBridge> {
    Arc::new(ItemInotifyBridge::new(PathBuf::from("/folder")))
}

/// A thread that takes what is queued as the bridge's own does, and passes on what it
/// took; kept by the bridge as its thread.
fn start_taking(bridge: &Arc<ItemInotifyBridge>) -> mpsc::Receiver<Vec<ItemAnnouncement>> {
    let (taken_sender, taken) = mpsc::channel();
    let taking = Arc::clone(bridge);
    let thread = std::thread::spawn(move || {
        while let Some(announcements) = taking.wait_for_announcements() {
            taken_sender.send(announcements).unwrap();
        }
    });
    *bridge.thread.lock().unwrap() = Some(thread);
    taken
}

#[test]
fn the_announcement_being_performed_is_what_the_bridges_own_lookup_is_answered_from() {
    let bridge = bridge();
    assert_eq!(bridge.find_in_progress(), None);
    bridge.mark_in_progress(Some(appeared(5)));
    assert_eq!(bridge.find_in_progress(), Some(appeared(5)));
    bridge.mark_in_progress(Some(appeared(6)));
    assert_eq!(bridge.find_in_progress(), Some(appeared(6)));
    bridge.mark_in_progress(None);
    assert_eq!(bridge.find_in_progress(), None);
    // The bridge's requests are told by the kernel's id of its thread.
    assert!(!bridge.is_own_thread(0) && !bridge.is_own_thread(77), "no thread yet: no request is its own");
    bridge.thread_id.set(77).unwrap();
    assert!(bridge.is_own_thread(77) && !bridge.is_own_thread(78));
}

#[test]
fn announcements_are_performed_in_the_order_they_were_queued() {
    let bridge = bridge();
    // Queued before the thread runs, as while the mount is built and not served.
    bridge.announce(vec![appeared(1), appeared(2)]);
    bridge.announce(Vec::new());
    bridge.announce(vec![appeared(3)]);
    assert_eq!(bridge.wait_for_announcements(), Some(vec![appeared(1), appeared(2)]), "the announcements of one change stay together");
    assert_eq!(bridge.wait_for_announcements(), Some(vec![appeared(3)]), "a change with nothing to announce queues nothing");
    // And while the thread waits for them.
    let taken = start_taking(&bridge);
    for number in 4..20 {
        bridge.announce(vec![appeared(number)]);
    }
    for number in 4..20 {
        assert_eq!(taken.recv().unwrap(), [appeared(number)]);
    }
    bridge.stop();
}

#[test]
fn a_stop_drops_what_is_queued_and_ends_the_thread() {
    let bridge = bridge();
    let taken = start_taking(&bridge);
    bridge.announce(vec![appeared(1)]);
    assert_eq!(taken.recv().unwrap(), [appeared(1)]);
    // The thread waits for the next; the stop ends its wait and waits for its end.
    bridge.stop();
    assert!(taken.recv().is_err(), "the thread ended");
    assert!(bridge.thread.lock().unwrap().is_none(), "and was waited for");
    // What comes later is dropped, and what was queued is gone.
    bridge.announce(vec![appeared(2)]);
    assert!(bridge.queued.lock().unwrap().is_empty());
    assert_eq!(bridge.wait_for_announcements(), None);
    bridge.stop();
    // Queued and never taken: dropped by the stop.
    let bridge = self::bridge();
    bridge.announce(vec![appeared(3)]);
    bridge.stop();
    assert!(bridge.queued.lock().unwrap().is_empty());
    assert_eq!(bridge.wait_for_announcements(), None);
}
