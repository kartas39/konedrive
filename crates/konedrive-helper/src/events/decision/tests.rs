use std::cell::RefCell;

use super::*;

/// What a file is said to be, and what the decision asked of it. No file,
/// no fanotify group and no helper state stand behind it.
struct Told {
    daemons_own: bool,
    state: fn() -> StateRead,
    item_id: fn() -> io::Result<Option<String>>,
    asked: RefCell<Vec<&'static str>>,
}

impl Told {
    /// A file in `state`, opened by somebody who is not its owner's daemon,
    /// with no item id.
    fn state(state: fn() -> StateRead) -> Self {
        Told { daemons_own: false, state, item_id: || Ok(None), asked: RefCell::new(Vec::new()) }
    }

    fn asked(&self) -> Vec<&'static str> {
        self.asked.borrow().clone()
    }
}

impl Facts for Told {
    fn daemons_own(&self) -> bool {
        self.asked.borrow_mut().push("daemon");
        self.daemons_own
    }

    fn state(&self) -> StateRead {
        self.asked.borrow_mut().push("state");
        (self.state)()
    }

    fn item_id(&self) -> io::Result<Option<String>> {
        self.asked.borrow_mut().push("item id");
        (self.item_id)()
    }
}

fn unreadable() -> io::Error {
    io::Error::from_raw_os_error(libc::EACCES)
}

/// Only a regular file can be a placeholder: a directory, a socket or a
/// device is let through with nothing asked about it — no table of the
/// helper's looked at, no attribute read.
#[test]
fn what_is_not_a_regular_file_is_allowed_and_nothing_is_asked() {
    let told = Told::state(|| Ok(Some(State::OnlineOnly)));
    assert!(matches!(Decision::of(false, &told), Decision::Allow));
    assert!(told.asked().is_empty(), "{:?}", told.asked());
}

/// The owning daemon's own open is let through whatever the file reads —
/// asking that daemon to hydrate a file it is blocked on opening is a
/// deadlock — and the file's attributes are not read for it.
#[test]
fn the_owning_daemons_own_open_is_allowed_before_the_state_is_read() {
    let told = Told { daemons_own: true, ..Told::state(|| Ok(Some(State::OnlineOnly))) };
    assert!(matches!(Decision::of(true, &told), Decision::Allow));
    assert_eq!(told.asked(), ["daemon"]);
}

/// A file that reads `hydrated` is let through with an ignore mark; the
/// item id is not read for it.
#[test]
fn a_hydrated_file_is_allowed_with_an_ignore_mark() {
    let told = Told::state(|| Ok(Some(State::Hydrated)));
    assert!(matches!(Decision::of(true, &told), Decision::AllowMarked));
    assert_eq!(told.asked(), ["daemon", "state"]);
}

/// Every other state is a file whose content is not known to be there:
/// its owner's daemon is asked. `hydrating` and `dehydrating` too — a file
/// caught half-way is not let through on the strength of what it was.
#[test]
fn a_file_in_any_other_state_goes_to_its_daemon() {
    let states: [fn() -> StateRead; 3] = [
        || Ok(Some(State::OnlineOnly)),
        || Ok(Some(State::Hydrating)),
        || Ok(Some(State::Dehydrating)),
    ];
    for state in states {
        let told = Told::state(state);
        let decision = Decision::of(true, &told);
        assert!(matches!(decision, Decision::Hydrate), "{:?}: {decision:?}", state());
        assert_eq!(told.asked(), ["daemon", "state"]);
    }
}

/// A file with neither attribute is not ours — a placeholder under
/// construction looks exactly like that — and is let through, without an
/// ignore mark: the mark would outlive the construction.
#[test]
fn a_file_with_no_state_and_no_item_id_is_allowed_without_a_mark() {
    let told = Told::state(|| Ok(None));
    assert!(matches!(Decision::of(true, &told), Decision::Allow));
    assert_eq!(told.asked(), ["daemon", "state", "item id"]);
}

/// An item id with no state is one of ours with its state missing: nothing
/// says its body is there, so it is denied, `EIO`, never allowed.
#[test]
fn a_file_with_an_item_id_and_no_state_is_denied() {
    let told = Told { item_id: || Ok(Some("ITEM!1".into())), ..Told::state(|| Ok(None)) };
    match Decision::of(true, &told) {
        Decision::Deny(why @ Denied::StateMissing { .. }) => {
            assert_eq!(why.errno(), libc::EIO);
            let Denied::StateMissing { item } = why else { unreachable!() };
            assert_eq!(item, "ITEM!1");
        }
        other => panic!("{other:?}"),
    }
}

/// What cannot be read is denied `EIO`, each for its own reason: an item id
/// that cannot be read where there is no state, a state that is no state,
/// and a state that cannot be read. None is ever allowed, and none is sent
/// to the daemon.
#[test]
fn a_file_whose_attributes_cannot_be_trusted_is_denied() {
    let told = Told { item_id: || Err(unreadable()), ..Told::state(|| Ok(None)) };
    let decision = Decision::of(true, &told);
    assert!(matches!(decision, Decision::Deny(Denied::ItemIdUnreadable(_))), "{decision:?}");

    let told = Told::state(|| Err(StateError::Corrupt("half".into())));
    let decision = Decision::of(true, &told);
    match &decision {
        Decision::Deny(Denied::StateCorrupt(value)) => assert_eq!(value, "half"),
        other => panic!("{other:?}"),
    }
    assert_eq!(told.asked(), ["daemon", "state"], "the item id is not read for it");

    let told = Told::state(|| Err(StateError::Io(unreadable())));
    let decision = Decision::of(true, &told);
    assert!(matches!(decision, Decision::Deny(Denied::StateUnreadable(_))), "{decision:?}");

    for why in [
        Denied::StateMissing { item: String::new() },
        Denied::ItemIdUnreadable(unreadable()),
        Denied::StateCorrupt(String::new()),
        Denied::StateUnreadable(unreadable()),
    ] {
        assert_eq!(why.errno(), libc::EIO, "{why:?}");
    }
}

/// The second decision, for a file that stopped reading `hydrated` while it
/// was being marked: made on what the file reads now, with the opener not
/// asked about again — a dehydration that began in between sends the open to
/// the daemon, and a file whose state went missing is denied or allowed by
/// its item id like any other.
#[test]
fn a_file_decided_again_is_decided_on_the_state_it_reads_now() {
    let told = Told { daemons_own: true, ..Told::state(|| panic!("the state is not read again")) };
    let decision = Decision::by_state(Ok(Some(State::Dehydrating)), &told);
    assert!(matches!(decision, Decision::Hydrate), "{decision:?}");
    assert!(told.asked().is_empty(), "{:?}", told.asked());

    let told = Told { item_id: || Ok(Some("ITEM!1".into())), ..Told::state(|| Ok(None)) };
    let decision = Decision::by_state(Ok(None), &told);
    assert!(matches!(decision, Decision::Deny(Denied::StateMissing { .. })), "{decision:?}");
    assert_eq!(told.asked(), ["item id"]);
}
