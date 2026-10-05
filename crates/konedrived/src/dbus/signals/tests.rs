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

/// `Overall` and `Trouble` go out when what they say changes, and only then: a change of
/// the published state, of the sign-in or of the downloads that leaves the account in the
/// same state for the same reason sends nothing.
#[tokio::test(start_paused = true)]
async fn the_overall_state_is_sent_when_it_changes_and_not_again_when_it_does_not() {
    use konedrive_dbus::overall::{Overall, Reason};
    use konedrive_dbus::FOLDER_INTERFACE_NAME;

    use crate::account::state::{SignInState, StateHandle};
    use crate::status::snapshot::{RootState, SyncTrouble, TroubleKind};

    let state = SyncStateHandle::new(SyncSnapshot::default());
    let account = StateHandle::new(AccountSnapshot::default());
    let transfers = Downloads::default();
    let sent: Arc<Mutex<Vec<Changed>>> = Arc::default();
    let log = Arc::clone(&sent);
    let (mut states, mut accounts, mut downloads) = (state.subscribe(), account.subscribe(), transfers.subscribe());
    let shown = seen(&mut states, &mut accounts, &mut downloads);
    tokio::spawn(decide(states, accounts, downloads, shown, move |changed| {
        log.lock().unwrap().push(changed);
        std::future::ready(())
    }));
    // The messages sent since the last call: each the properties of `Folder` it carries.
    let mut read = 0;
    let mut since = |sent: &Mutex<Vec<Changed>>| -> Vec<Vec<(&'static str, Value<'static>)>> {
        let sent = sent.lock().unwrap();
        let new = sent[read..]
            .iter()
            .map(|message| {
                assert_eq!(message.keys().copied().collect::<Vec<_>>(), [FOLDER_INTERFACE_NAME]);
                let mut properties: Vec<_> = message[FOLDER_INTERFACE_NAME].iter().map(|(name, value)| (*name, value.try_clone().unwrap())).collect();
                properties.sort_by_key(|(name, _)| *name);
                properties
            })
            .collect();
        read = sent.len();
        new
    };
    let settle = || tokio::time::sleep(Duration::from_millis(10));
    let overall = |reason: Reason| ("Overall", Value::from(Overall::from(reason)));

    // Signed out, with no folder: a folder coming up changes nothing of the whole.
    state.update(|s| {
        s.folder.root_path = "/home/u/OneDrive".into();
        s.folder.root_state = RootState::Ready;
        s.folder.helper_state = HelperState::Connected;
    });
    settle().await;
    assert!(since(&sent).is_empty(), "still signed out");

    account.update(|s| s.state = SignInState::SignedIn);
    settle().await;
    assert_eq!(since(&sent), [vec![overall(Reason::UpToDate)]]);

    // What does not change the state or the reason sends nothing: a counter, the account's
    // name, a check with OneDrive.
    state.update(|s| {
        s.cycle.items_listed = 7;
        s.cycle.last_checked = 1_700_000_000;
    });
    account.update(|s| s.display_name = "Somebody".into());
    settle().await;
    assert!(since(&sent).is_empty());

    // A download: sent when the first starts, and not again while it moves or a second joins.
    let first = transfers.start("/home/u/OneDrive/a".into(), 100);
    settle().await;
    assert_eq!(since(&sent), [vec![overall(Reason::Transferring)]]);
    first.progress(50, 100);
    let second = transfers.start("/home/u/OneDrive/b".into(), 100);
    settle().await;
    drop(first);
    settle().await;
    assert!(since(&sent).is_empty(), "still transferring");
    // A change waits to upload as the last download ends: transferring for another cause.
    state.update(|s| s.outbox.pending_count = 1);
    drop(second);
    settle().await;
    assert!(since(&sent).is_empty(), "the same state for the same reason");

    // Trouble: the reason and its sentence in one message; a reworded sentence alone
    // sends `Trouble` only.
    let trouble = |text: &str| Some(SyncTrouble { text: text.into(), blocking: false, kind: TroubleKind::Unreachable });
    state.update(|s| s.cycle.sync_trouble = trouble("cannot reach OneDrive (timed out); trying again"));
    settle().await;
    assert_eq!(since(&sent), [vec![overall(Reason::Unreachable), ("Trouble", Value::from("cannot reach OneDrive (timed out); trying again"))]]);
    state.update(|s| s.cycle.sync_trouble = trouble("cannot reach OneDrive (refused); trying again"));
    settle().await;
    assert_eq!(since(&sent), [vec![("Trouble", Value::from("cannot reach OneDrive (refused); trying again"))]]);

    // Back to what it was, each change by itself.
    state.update(|s| s.cycle.sync_trouble = None);
    settle().await;
    state.update(|s| s.outbox.pending_count = 0);
    settle().await;
    assert_eq!(since(&sent), [vec![overall(Reason::Transferring), ("Trouble", Value::from(""))], vec![overall(Reason::UpToDate)]]);
}
