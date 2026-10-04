use std::collections::BTreeSet;

use zbus::object_server::Interface;

use super::*;
use crate::dbus::{Conflicts, Folder, LocalScan, Transfers, UploadQueue};

/// The names of the properties `I` has on the bus, read from what it says of itself.
fn on_the_bus<I: Interface>(interface: &I) -> BTreeSet<String> {
    let mut xml = String::new();
    interface.introspect_to_writer(&mut xml, 0);
    xml.lines()
        .filter_map(|line| line.trim().strip_prefix("<property name=\""))
        .filter_map(|rest| rest.split('"').next())
        .map(str::to_owned)
        .collect()
}

/// Every property a folder's interface has on the bus is a row of the table under that
/// interface's name, so it is announced when it changes; the few that are not are named
/// here, with who announces them. And no row names a property that is not there.
#[tokio::test]
async fn every_property_on_the_bus_is_a_row_of_the_table() {
    let service = crate::sync::testing::wiring().build();
    let rows = |interface: &str| -> BTreeSet<String> {
        AT_ONCE.iter().chain(COALESCED).filter(|row| row.interface() == interface).map(|row| row.name().to_owned()).collect()
    };
    let with = |rows: BTreeSet<String>, others: &[&str]| -> BTreeSet<String> { rows.into_iter().chain(others.iter().map(|name| (*name).to_owned())).collect() };

    // `Source` goes out with `Path`; the two settings by the call that sets them.
    assert_eq!(on_the_bus(&Folder::new(service.clone())), with(rows(FOLDER), &["Source", "IgnorePatterns", "Thumbnails"]));
    assert_eq!(on_the_bus(&UploadQueue::new(service.clone())), rows(QUEUE));
    assert_eq!(on_the_bus(&Transfers::new(service.clone())), rows(MOVING));
    assert_eq!(on_the_bus(&LocalScan::new(service.clone())), rows(SCAN));
    // `MachineName` is `config.toml`'s, read when the daemon starts.
    assert_eq!(on_the_bus(&Conflicts::new(service.clone())), with(rows(CONFLICTS_INTERFACE_NAME), &["MachineName"]));
    let listed: usize = [FOLDER, QUEUE, MOVING, SCAN, CONFLICTS_INTERFACE_NAME].iter().map(|interface| rows(interface).len()).sum();
    assert_eq!(listed, AT_ONCE.len() + COALESCED.len(), "no row is under an interface the folder does not have, and no name is there twice");
}

/// What is announced is what changed, with its value now, under the interface that holds
/// it; a change of nothing announces nothing.
#[test]
fn what_changed_is_announced_with_its_value_under_its_interface() {
    let old = Seen::default();
    let mut new = old.clone();
    new.snapshot.cycle.items_listed = 7;
    new.snapshot.outbox.pending_count = 2;
    new.snapshot.pause.paused_until = Some(0);

    let coalesced = changed(COALESCED, &old, &new);
    assert_eq!(coalesced.keys().copied().collect::<Vec<_>>(), [FOLDER, QUEUE]);
    assert_eq!(coalesced[FOLDER].get("ItemsListed"), Some(&Value::from(7u64)));
    assert_eq!(coalesced[FOLDER].len(), 1);
    assert_eq!(coalesced[QUEUE].get("PendingCount"), Some(&Value::from(2u32)));

    // A pause until resumed: `Paused` turns, `PausedUntil` stays 0 and is not sent.
    let at_once = changed(AT_ONCE, &old, &new);
    assert_eq!(at_once[FOLDER].get("Paused"), Some(&Value::from(true)));
    assert_eq!(at_once[FOLDER].len(), 1);
    assert!(changed(AT_ONCE, &new, &new).is_empty() && changed(COALESCED, &new, &new).is_empty());
}
