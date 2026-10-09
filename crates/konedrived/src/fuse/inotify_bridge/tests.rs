use std::ffi::OsStr;
use std::sync::mpsc::TryRecvError;
use std::sync::Arc;

use konedrive_tree::Kind;

use super::{find_name_before_change, Announcement, AnnouncementInProgress, InotifyBridge, NameBeforeChange};
use crate::fuse::node_table::NodeId;
use crate::fuse::write_operations::tests::folder;

fn appearing(parent: u64, name: &str) -> AnnouncementInProgress {
    AnnouncementInProgress::NameAppearing { parent: NodeId(parent), name: name.into() }
}

fn vanishing(parent: u64, name: &str, node_id: u64, kind: Kind) -> AnnouncementInProgress {
    AnnouncementInProgress::NameVanishing { parent: NodeId(parent), name: name.into(), node_id: NodeId(node_id), kind }
}

fn before(in_progress: Option<&AnnouncementInProgress>, parent: u64, name: &str) -> Option<NameBeforeChange> {
    find_name_before_change(in_progress, NodeId(parent), OsStr::new(name))
}

#[test]
fn with_no_announcement_in_progress_every_name_is_as_the_folder_has_it() {
    assert_eq!(before(None, 1, "a.txt"), None);
}

/// The store has the name already; the kernel passes the create on only if the name is not
/// there yet.
#[test]
fn a_name_that_is_appearing_was_absent() {
    let in_progress = appearing(4, "new.txt");
    assert_eq!(before(Some(&in_progress), 4, "new.txt"), Some(NameBeforeChange::Absent));
}

/// The store has the name no more; the kernel passes the remove on only if the name is
/// still there, and needs its node and its kind.
#[test]
fn a_name_that_is_vanishing_was_present_with_its_node_and_kind() {
    let file = vanishing(4, "gone.txt", 9, Kind::File);
    assert_eq!(before(Some(&file), 4, "gone.txt"), Some(NameBeforeChange::Present { node_id: NodeId(9), kind: Kind::File }));
    let directory = vanishing(1, "gone", 12, Kind::Folder);
    assert_eq!(before(Some(&directory), 1, "gone"), Some(NameBeforeChange::Present { node_id: NodeId(12), kind: Kind::Folder }));
}

#[test]
fn another_name_or_the_same_name_in_another_directory_is_as_the_folder_has_it() {
    for in_progress in [appearing(4, "new.txt"), vanishing(4, "new.txt", 9, Kind::File)] {
        assert_eq!(before(Some(&in_progress), 4, "other.txt"), None);
        assert_eq!(before(Some(&in_progress), 5, "new.txt"), None);
    }
}

/// A bridge over the fixture's folder, whose thread is not started: no mount answers it.
fn bridge() -> InotifyBridge {
    let folder = folder(Vec::new());
    InotifyBridge::new(folder.disk.clone(), Arc::clone(&folder.node_resolver), Arc::clone(&folder.directory_contents))
}

fn shown_changed(rel: &str, id: &str) -> Announcement {
    Announcement::FileShownChanged { rel: rel.into(), id: id.into() }
}

/// The mount does not answer yet: what is given to announce waits, in the order it came, for
/// the thread that is started once it does.
#[test]
fn what_is_announced_before_the_thread_is_started_waits_in_its_order() {
    let bridge = bridge();
    bridge.announce(vec![shown_changed("a.txt", "A"), shown_changed("b.txt", "B")]);
    bridge.announce(vec![shown_changed("docs/c.txt", "C")]);
    let queued = crate::panic::lock(&bridge.queued).take().expect("the queue of a bridge that is not started");
    assert_eq!(queued.try_recv(), Ok(vec![shown_changed("a.txt", "A"), shown_changed("b.txt", "B")]));
    assert_eq!(queued.try_recv(), Ok(vec![shown_changed("docs/c.txt", "C")]));
    assert_eq!(queued.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn nothing_is_queued_for_an_announcement_of_nothing() {
    let bridge = bridge();
    bridge.announce(Vec::new());
    let queued = crate::panic::lock(&bridge.queued).take().expect("the queue of a bridge that is not started");
    assert_eq!(queued.try_recv(), Err(TryRecvError::Empty));
}

/// What was queued and not announced is abandoned with the queue, and what comes later is
/// dropped: the way to the thread is gone.
#[test]
fn what_is_announced_after_the_bridge_is_stopped_is_dropped() {
    let bridge = bridge();
    let queued = crate::panic::lock(&bridge.queued).take().expect("the queue of a bridge that is not started");
    bridge.stop();
    bridge.announce(vec![shown_changed("a.txt", "A")]);
    assert_eq!(queued.try_recv(), Err(TryRecvError::Disconnected));
}

/// A request is the bridge's own by the id of its thread, which no request has before the
/// thread runs.
#[test]
fn no_request_is_the_bridges_own_before_its_thread_runs() {
    let bridge = bridge();
    assert!(!bridge.is_own_thread(1) && !bridge.is_own_thread(std::process::id()));
}

/// What the tests of the parts that announce through a bridge ask of one whose thread is not
/// started.
impl InotifyBridge {
    /// What waits to be announced, in its order.
    pub(in crate::fuse) fn list_queued_announcements(&self) -> Vec<Announcement> {
        crate::panic::lock(&self.queued).as_ref().map(|queued| queued.try_iter().flatten().collect()).unwrap_or_default()
    }

    /// The bridge's thread is the caller's from now on, and it announces `in_progress`.
    pub(in crate::fuse) fn begin_announcing_on_this_thread(&self, in_progress: AnnouncementInProgress) {
        // SAFETY: a system call that takes nothing and cannot fail.
        self.thread_id.store(unsafe { libc::gettid() }, std::sync::atomic::Ordering::SeqCst);
        *crate::panic::lock(&self.in_progress) = Some(in_progress);
    }
}
