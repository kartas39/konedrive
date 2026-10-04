use std::sync::Mutex;

use konedrive_dbus::rows::Transfer;
use zbus::zvariant::Value;

use super::*;
use crate::status::transfers::Transfers as Downloads;
use crate::status::snapshot::SyncStateHandle;

/// A download moves its `Downloads` entry on with every
/// read, and a listing the counters with every page, but what goes on
/// the bus is at most four messages a second — each carrying everything
/// that changed — and the last value always arrives. A hundred changes
/// in one second, counted on the paused clock.
#[tokio::test(start_paused = true)]
async fn a_hundred_changes_in_a_second_are_at_most_five_messages() {
    let state = SyncStateHandle::new(SyncSnapshot::default());
    let transfers = Downloads::default();
    let sent: Arc<Mutex<Vec<Changed>>> = Arc::default();
    let log = Arc::clone(&sent);
    tokio::spawn(coalesce(state.subscribe(), transfers.subscribe(), Seen::default(), move |changed| {
        log.lock().unwrap().push(changed);
        std::future::ready(())
    }));
    // What a client that applies every message holds.
    let held = |sent: &[Changed]| -> BTreeMap<&'static str, Value<'static>> {
        sent.iter().flat_map(|message| message.values().flatten()).map(|(name, value)| (*name, value.try_clone().unwrap())).collect()
    };
    let downloads = |list: Vec<Transfer>| Value::from(list);

    let entry = transfers.start("/r/f.bin".into(), 1000);
    for step in 1..=100u64 {
        entry.progress(step * 10, 1000);
        if step % 10 == 0 {
            state.update(|s| s.cycle.items_listed = step);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let messages = sent.lock().unwrap().len();
    assert!((2..=5).contains(&messages), "{messages} messages for 100 changes in one second");

    tokio::time::sleep(COALESCE * 2).await;
    let last = held(&sent.lock().unwrap());
    assert_eq!(last["Downloads"], downloads(vec![Transfer { path: "/r/f.bin".into(), done: 1000, total: 1000 }]));
    assert_eq!(last["ItemsListed"], Value::from(100u64));

    drop(entry);
    tokio::time::sleep(COALESCE * 2).await;
    assert_eq!(held(&sent.lock().unwrap())["Downloads"], downloads(Vec::new()), "an ended download leaves the list");
}
