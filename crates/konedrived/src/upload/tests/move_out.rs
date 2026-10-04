//! Moves out of the folder (`docs/design/writes.md` §8, §10, §12), on the host: each case of
//! the move out once, through the worker, against the fake OneDrive — a folder placed by the
//! real materializer, rows made by the real examination, the content downloaded from the fake
//! OneDrive and verified by the real fill — and a fake helper that finds an object by its
//! handle wherever it stands beside the folder, as the real one answers: `ESTALE` for what is
//! gone, `EPERM` for an object without the item id. What needs the real helper (the marks
//! themselves) is in the VM suite (`tests/vm/scenarios/move_out.rs`).

use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Mutex;

use async_trait::async_trait;

use super::*;
use crate::helper::linked::Helper;
use crate::helper::{Clearance, HelperError};
use crate::hydration::graph_source::GraphSource;
use crate::hydration::source::FillError;
use crate::local::liveness::answered;
use crate::local::Whereabouts;
use crate::upload::move_out::{trash_of, Filler, MoveOuts, SourceFill, Tidy, CONTENT_LOCAL};

/// A helper that answers `OpenByHandle` by looking for the object beneath one directory:
/// the folder, and everything that left it, are there.
pub(super) struct FakeHelper {
    beneath: PathBuf,
    /// Every `OpenByHandle` is answered with this refusal, while set.
    refuse: Mutex<Option<i32>>,
    /// `NotRunning`, while set.
    down: Mutex<bool>,
    /// What was asked: (call, where the object was).
    calls: Mutex<Vec<(&'static str, PathBuf)>>,
    /// How many times an object was asked for by its handle, whatever the answer.
    asked: std::sync::atomic::AtomicUsize,
}

fn where_is(file: &File) -> PathBuf {
    std::fs::read_link(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(file))).unwrap_or_default()
}

fn handle_of(path: &Path) -> Option<FileHandle> {
    FileHandle::at(&File::open(path.parent()?).ok()?, path.file_name()?).ok()
}

impl FakeHelper {
    pub(super) fn beneath(beneath: PathBuf) -> Self {
        Self { beneath, refuse: Mutex::new(None), down: Mutex::new(false), calls: Mutex::new(Vec::new()), asked: Default::default() }
    }

    /// Where the object with `handle` stands now, if anywhere.
    fn find(&self, handle: &FileHandle) -> Option<PathBuf> {
        let mut dirs = vec![self.beneath.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if handle_of(&path).as_ref() == Some(handle) {
                    return Some(path);
                }
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    dirs.push(path);
                }
            }
        }
        None
    }

    fn log(&self, call: &'static str, file: &File) {
        self.calls.lock().unwrap().push((call, where_is(file)));
    }

    fn called(&self, call: &str) -> Vec<PathBuf> {
        self.calls.lock().unwrap().iter().filter(|(c, _)| *c == call).map(|(_, p)| p.clone()).collect()
    }

    fn up(&self) -> Result<(), HelperError> {
        if *self.down.lock().unwrap() {
            return Err(HelperError::NotRunning);
        }
        Ok(())
    }
}

#[async_trait]
impl Helper for FakeHelper {
    async fn open_by_handle(&self, _dir: &File, handle: &FileHandle) -> Result<OwnedFd, HelperError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.up()?;
        if let Some(errno) = *self.refuse.lock().unwrap() {
            return Err(HelperError::Refused(errno));
        }
        let stale = || HelperError::Refused(libc::ESTALE);
        let path = self.find(handle).ok_or_else(stale)?;
        let meta = std::fs::symlink_metadata(&path).map_err(|_| stale())?;
        let file = if meta.is_dir() {
            File::open(&path)
        } else {
            std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW).open(&path)
        }
        .map_err(|_| stale())?;
        if FileHandle::of(&file).ok().as_ref() != Some(handle) {
            return Err(stale());
        }
        if xattr::get(&path, XATTR_ITEM_ID).ok().flatten().is_none() {
            return Err(HelperError::Refused(libc::EPERM));
        }
        self.log("open", &file);
        Ok(file.into())
    }

    async fn mark_file(&self, file: &File) -> Result<(), HelperError> {
        self.up()?;
        self.log("mark_file", file);
        Ok(())
    }

    async fn mark_dir(&self, dir: &File) -> Result<(), HelperError> {
        self.up()?;
        self.log("mark_dir", dir);
        Ok(())
    }

    async fn unmark_dir(&self, dir: &File) -> Result<(), HelperError> {
        self.up()?;
        self.log("unmark_dir", dir);
        Ok(())
    }

    fn clearance(&self) -> Option<Clearance> {
        // No helper runs on the host: the way is clear.
        Some(Clearance::NoLink(self.beneath.join("no-helper.sock")))
    }
}

/// A [`World`] of `items` — (id, parent or the root, name, with `/` for a folder, content) —
/// whose worker has what move-outs need: the fake helper, and fills from the fake OneDrive,
/// which holds each file's content.
fn leaving(items: &[(&str, Option<&str>, &str, &[u8])]) -> World {
    let changes: Vec<Change> = items
        .iter()
        .map(|(id, parent, name, content)| match name.strip_suffix('/') {
            Some(name) => folder(id, parent.unwrap_or("R"), name),
            None => file(id, parent.unwrap_or("R"), name, content),
        })
        .collect();
    let w = World::new(&changes);
    w.cloud(|c| {
        for (id, _, _, content) in items.iter().filter(|(_, _, name, _)| !name.ends_with('/')) {
            c.items.get_mut(*id).unwrap().content = content.to_vec();
        }
    });
    w.fills_with(Arc::new(SourceFill(Arc::new(GraphSource::new(w.h.graph.client())))));
    w
}

impl World {
    fn fills_with(&self, filler: Arc<dyn Filler>) {
        let roots = self.h.moved_out.lock().unwrap().as_ref().map(|mo| Arc::clone(&mo.roots));
        let root = self.root.path.clone();
        *self.h.moved_out.lock().unwrap() = Some(MoveOuts {
            helper: self.helper.clone(),
            filler,
            route: None,
            home_trash: Some(self.trash()),
            roots: roots.unwrap_or_else(|| Arc::new(move || vec![root.clone()])),
        });
    }

    /// A fill of the real kind, from the fake OneDrive.
    fn onedrive_fill(&self) -> SourceFill {
        SourceFill(Arc::new(GraphSource::new(self.h.graph.client())))
    }

    /// OneDrive answers the download of `id` with these bytes, which are not the item's: the
    /// fill's check of the hash refuses them.
    fn serves(&self, id: &str, content: &[u8]) {
        self.cloud(|c| c.items.get_mut(id).unwrap().content = content.to_vec());
    }

    /// How many downloads OneDrive was asked for.
    fn downloads(&self) -> usize {
        self.cloud(|c| c.count("GET", "dl/"))
    }

    /// Beside the folder: where things go that leave it.
    fn beside(&self, rel: &str) -> PathBuf {
        self.dir.path().canonicalize().unwrap().join(rel)
    }

    /// The user's own Trash, as `$XDG_DATA_HOME/Trash` would be.
    fn trash(&self) -> PathBuf {
        self.beside("Trash")
    }

    /// `rel` moved out of the folder, to `to`: the examination's liveness is told where it went.
    fn move_out(&self, rel: &str, to: &Path) -> FileHandle {
        let handle = self.handle(rel);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::rename(self.path(rel), to).unwrap();
        self.liveness.alive_tree(to);
        handle
    }

    /// `rel` sent to the Trash as a desktop sends it: its `.trashinfo` first. Where it went.
    fn to_trash(&self, rel: &str) -> PathBuf {
        let name = Path::new(rel).file_name().unwrap().to_str().unwrap().to_owned();
        std::fs::create_dir_all(self.trash().join("info")).unwrap();
        std::fs::write(self.trash().join(format!("info/{name}.trashinfo")), "[Trash Info]\n").unwrap();
        let to = self.trash().join("files").join(&name);
        self.move_out(rel, &to);
        to
    }

    fn deletes(&self) -> usize {
        self.cloud(|c| c.count("DELETE", "items/"))
    }

    fn in_bin(&self, id: &str) -> bool {
        self.cloud(|c| c.bin.contains_key(id) && !c.items.contains_key(id))
    }

    /// Rows in backoff are due now, as after a restart that waited long enough.
    fn due_now(&self) {
        self.store.call_blocking(move |s| s.outbox_retry_now()).unwrap();
    }

    /// The reason each row waits with, by its item.
    fn reasons(&self) -> Vec<(String, Option<String>)> {
        self.rows().into_iter().map(|r| (r.item_id.clone().unwrap_or_default(), r.reason_text())).collect()
    }

    /// What `dropped` left outside the folder tidied, as a drop with no worker tidies it.
    fn tidy(&self, dropped: &[OutboxRow]) {
        let mo = self.h.moved_out.lock().unwrap().clone().unwrap();
        self.h.runtime.block_on(Tidy { mo: &mo, root: &self.root, store: &self.store, locks: &self.locks }.dropped(dropped));
    }

    /// Another account's registered folder, `name` beside this one, with every folder there is.
    fn another_folder(&self, name: &str) -> PathBuf {
        let other = self.beside(name);
        std::fs::create_dir_all(&other).unwrap();
        let roots = vec![self.root.path.clone(), other.clone()];
        self.h.moved_out.lock().unwrap().as_mut().unwrap().roots = Arc::new(move || roots.clone());
        other
    }
}

fn konedrive_attrs(path: &Path) -> Vec<String> {
    xattr::list(path).unwrap().filter_map(|n| n.to_str().map(str::to_owned)).filter(|n| n.starts_with("user.konedrive.")).collect()
}

fn state(path: &Path) -> Option<State> {
    placeholder::read_state(&File::open(path).unwrap()).unwrap()
}

/// Without what move-outs need (no helper link yet), a `move-out` row is not taken: nothing is
/// downloaded, stripped or deleted, and the row is there for the worker that has it.
#[test]
fn a_move_out_waits_while_the_worker_has_no_helper_for_it() {
    let w = World::new(&[file("P", "R", "p.txt", b"p")]);
    let to = w.beside("outside/p.txt");
    w.move_out("p.txt", &to);
    w.examine(&[("", "p.txt")]);
    w.run();
    assert_eq!(w.summary(), vec![(OutboxKind::MoveOut, "p.txt".into(), OutboxState::Ready)]);
    assert_eq!((w.deletes(), state(&to)), (0, Some(State::OnlineOnly)));
    assert!(!konedrive_attrs(&to).is_empty());
}

/// §4.6, WR5: a placeholder moved anywhere but the Trash is marked again, downloaded where it
/// went, stripped of konedrive's attributes, and only then deleted in OneDrive.
#[test]
fn a_placeholder_moved_out_is_downloaded_where_it_went_then_deleted() {
    let w = leaving(&[("P", None, "p.txt", b"the content")]);
    let to = w.beside("outside/p.txt");
    w.move_out("p.txt", &to);
    w.examine(&[("", "p.txt")]);
    let rows = w.rows();
    assert_eq!(rows.iter().map(|r| r.kind).collect::<Vec<_>>(), vec![OutboxKind::MoveOut]);
    assert_eq!(state(&to), Some(State::OnlineOnly));

    w.run();
    assert_eq!(std::fs::read(&to).unwrap(), b"the content", "downloaded where it went");
    assert!(konedrive_attrs(&to).is_empty(), "an ordinary file now: {:?}", konedrive_attrs(&to));
    let marked = w.helper.called("mark_file");
    assert!(!marked.is_empty() && marked.iter().all(|p| p == &to), "marked again before the download: {marked:?}");
    assert!(w.in_bin("P"), "the item went to OneDrive's recycle bin");
    assert!(w.rows().is_empty());
    assert!(w.base("P").is_none());
}

/// §5, WR5: the item is deleted only once the file outside holds OneDrive's content, checked
/// against its hash. A download that brings other bytes deletes nothing and leaves the file not
/// downloaded; the row stays, and the next run — a restart — downloads it and only then deletes.
#[test]
fn a_download_that_is_not_the_items_content_deletes_nothing() {
    let w = leaving(&[("P", None, "p.bin", &[7u8; 4096])]);
    w.serves("P", &[8u8; 4096]);
    let to = w.beside("outside/p.bin");
    w.move_out("p.bin", &to);
    w.examine(&[("", "p.bin")]);

    w.run();
    assert_eq!(w.deletes(), 0, "nothing is deleted while the content is not the item's");
    assert_ne!(state(&to), Some(State::Hydrated), "not taken for downloaded");
    let rows = w.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].kind, rows[0].state), (OutboxKind::MoveOut, OutboxState::Retry));
    assert!(rows[0].reason_text().as_deref().is_some_and(|r| r.starts_with(Reason::Download(None).key())), "{:?}", rows[0].reason);
    assert_eq!(rows[0].snapshot, None, "not marked local");
    assert!(!konedrive_attrs(&to).is_empty(), "nothing stripped");

    w.serves("P", &[7u8; 4096]);
    w.due_now();
    w.run();
    assert_eq!(std::fs::read(&to).unwrap(), vec![7u8; 4096]);
    assert!(w.in_bin("P"));
    assert!(w.rows().is_empty());
}

/// F90: an object the helper does not hand over is never taken for gone, whatever the answer:
/// the row stays, with the reason, and nothing is deleted in OneDrive. Neither is one stripped
/// by someone else where it went (`EPERM`, with no marker of this row's to explain it).
#[test]
fn an_object_that_cannot_be_reached_is_never_taken_for_gone() {
    let w = leaving(&[("P", None, "p.txt", b"p")]);
    let to = w.beside("outside/p.txt");
    w.move_out("p.txt", &to);
    w.examine(&[("", "p.txt")]);
    let answers = [
        (libc::EPERM, Reason::Unreachable(None).key().to_owned(), OutboxState::Retry),
        (libc::EAGAIN, Reason::NotLocal.key().to_owned(), OutboxState::Retry),
        (libc::EIO, Reason::Unreachable(Some(format!("errno {}", libc::EIO))).key().to_owned(), OutboxState::Retry),
        (libc::EINVAL, Reason::BadHandle.key().to_owned(), OutboxState::Blocked),
    ];
    for (errno, reason, state) in answers {
        *w.helper.refuse.lock().unwrap() = Some(errno);
        w.due_now();
        w.store.call_blocking(move |s| s.outbox_unblock(&[Reason::BadHandle])).unwrap();
        w.run();
        let row = w.rows().remove(0);
        assert_eq!((row.reason.as_ref().map(|r| r.key().to_owned()), row.state), (Some(reason), state), "errno {errno}");
    }
    w.store.call_blocking(move |s| s.outbox_unblock(&[Reason::BadHandle])).unwrap();

    // A helper that does not answer decides nothing either.
    *w.helper.refuse.lock().unwrap() = None;
    *w.helper.down.lock().unwrap() = true;
    w.due_now();
    w.run();
    assert_eq!(reason_of(&w, "p.txt").as_deref(), Some(Reason::NoHelper.key()));

    // Its item id taken off by someone else: the helper's own `EPERM`.
    *w.helper.down.lock().unwrap() = false;
    xattr::remove(&to, XATTR_ITEM_ID).unwrap();
    w.due_now();
    w.run();
    assert_eq!(reason_of(&w, "p.txt").as_deref(), Some(Reason::Unreachable(None).key()));
    assert_eq!((w.deletes(), w.downloads()), (0, 0));
    assert!(to.exists() && w.base("P").is_some());
}

/// §5: an object deleted by the user after it left is gone, and its item is deleted — a file,
/// and a folder with what it held — but only when the helper says so twice, and with nothing
/// where the object was last proved to be: an inode that cannot be read answers `ESTALE` every
/// time, and stands there.
#[test]
fn gone_is_believed_twice_and_with_nothing_where_the_object_was() {
    let w = leaving(&[("P", None, "p.txt", b"p"), ("D", None, "d/", b""), ("D1", Some("D"), "one.txt", b"one")]);
    let (p, d) = (w.beside("outside/p.txt"), w.beside("outside/d"));
    w.move_out("p.txt", &p);
    w.move_out("d", &d);
    w.examine(&[("", "p.txt"), ("", "d")]);
    *w.helper.refuse.lock().unwrap() = Some(libc::ESTALE);
    w.run();
    assert!(w.reasons().iter().all(|(_, r)| r.as_deref() == Some(Reason::GoneOnce.key())), "one ESTALE is not enough: {:?}", w.reasons());
    w.due_now();
    w.run();
    assert_eq!(w.deletes(), 0);
    assert!(w.reasons().iter().all(|(_, r)| r.as_deref() == Some(Reason::GoneUnproved.key())), "they still stand there: {:?}", w.reasons());

    *w.helper.refuse.lock().unwrap() = None;
    std::fs::remove_file(&p).unwrap();
    std::fs::remove_dir_all(&d).unwrap();
    w.due_now();
    w.run();
    assert_eq!(w.deletes(), 0, "one ESTALE is not enough");
    w.due_now();
    w.run();
    assert!(w.in_bin("P") && w.in_bin("D") && w.in_bin("D1"));
    assert_eq!(w.deletes(), 2, "one DELETE for the file, one for the folder");
    assert!(w.rows().is_empty(), "{:?}", w.summary());
}

/// Every failure to decode a handle is `ESTALE`, so a handle taken on
/// another filesystem than the folder's now (a new disk, a snapshot rolled back) says nothing. The
/// record of that filesystem is the root's own handle (not `f_fsid`, a device number on XFS). A
/// row meeting a changed record waits; the next examination takes every handle again — a Full
/// scan: what is missing is placed again rather than deleted, and a `move-out` takes the handle
/// of what stands where it went — and the row then runs as any other.
#[test]
fn a_changed_filesystem_takes_the_handles_again_and_deletes_nothing() {
    use crate::local::liveness::handle_namespace;
    let w = leaving(&[("P", None, "p.txt", b"moved"), ("Q", None, "q.txt", b"q")]);
    let p = w.beside("outside/p.txt");
    let p_handle = w.move_out("p.txt", &p);
    w.examine(&[("", "p.txt")]);
    let row = w.rows()[0].clone();
    assert_eq!(row.target_name.as_deref(), p.to_str(), "where it went is kept");
    let root = File::open(&w.root.path).unwrap();
    let recorded = w.store.call_blocking(move |s| s.handles_filesystem()).unwrap().unwrap();
    assert_eq!(recorded, handle_namespace(&root).unwrap());
    assert!(recorded.starts_with("root:"), "keyed on the root's handle: {recorded}");

    // The filesystem changed: P's recorded handle is from the old one and no longer found.
    w.store.call_blocking(move |s| s.set_handles_filesystem("root:0102")).unwrap();
    let stale = FileHandle { kind: p_handle.kind, bytes: vec![0; p_handle.bytes.len()] };
    w.store.call_blocking(move |s| s.outbox_amend(row.seq, |r| r.inode.as_mut().unwrap().handle = Some(stale.clone()))).unwrap();
    for _ in 0..2 {
        w.run();
        w.due_now();
    }
    assert_eq!(w.deletes(), 0);
    assert_eq!(w.rows()[0].reason_text().as_deref(), Some(Reason::StaleHandle.key()));

    // The re-marking does not take such an answer for "gone": it asks again at every look
    // of the same worker (paused here, so that only the re-marking asks).
    let engine = w.h.engine();
    pause(&engine);
    let before = w.helper.asked.load(Ordering::SeqCst);
    w.h.drain(&engine);
    w.h.drain(&engine);
    assert_eq!(w.helper.asked.load(Ordering::SeqCst) - before, 2, "asked again at the next look");
    resume(&engine);

    // The examination takes the handles again: Q, missing meanwhile, is placed again, not deleted.
    std::fs::rename(w.path("q.txt"), w.beside("gone-q.txt")).unwrap();
    let examined = w.examine(&[("", "q.txt")]);
    assert!(examined.renewed);
    assert_eq!(examined.unproven, vec!["Q".to_owned()]);
    assert_eq!(w.store.call_blocking(move |s| s.handles_filesystem()).unwrap(), Some(recorded));
    assert_eq!(w.rows().len(), 1);
    assert_eq!(w.rows()[0].inode.as_ref().unwrap().handle.as_ref(), Some(&p_handle), "found again where it went");

    w.due_now();
    w.run();
    assert_eq!(std::fs::read(&p).unwrap(), b"moved");
    assert!(w.in_bin("P") && !w.in_bin("Q"));
}

/// In a folder sent to the Trash, the placeholders go before anything is stripped, so a removal
/// that fails leaves nothing stripped, and nothing is deleted in OneDrive until it went through.
#[test]
fn a_trashed_folder_strips_nothing_until_its_placeholders_are_gone() {
    use std::os::unix::fs::PermissionsExt;
    let w = leaving(&[("K", None, "kept/", b""), ("K1", Some("K"), "a-down.txt", b"downloaded"), ("K2", Some("K"), "b-cloud.txt", b"cloud")]);
    w.hydrate("kept/a-down.txt", b"downloaded");
    let kept = w.to_trash("kept");
    w.examine(&[("", "kept")]);
    std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o555)).unwrap();
    w.run();
    std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(w.deletes(), 0);
    assert!(kept.join("b-cloud.txt").exists());
    assert!(!konedrive_attrs(&kept.join("a-down.txt")).is_empty(), "nothing stripped before the placeholders went");

    w.due_now();
    w.run();
    assert!(!kept.join("b-cloud.txt").exists());
    assert!(konedrive_attrs(&kept.join("a-down.txt")).is_empty());
    assert!(w.in_bin("K"));
    assert_eq!(w.downloads(), 0);
}

/// A crash between two strips of a folder converges: what was stripped already answers
/// `EPERM`, which the row's marker explains, and the folder goes once the rest is stripped.
#[test]
fn a_crash_between_two_strips_of_a_folder_converges() {
    let w = leaving(&[("D", None, "d/", b""), ("P1", Some("D"), "one.txt", b"first"), ("P2", Some("D"), "two.txt", b"second")]);
    let to = w.beside("outside/d");
    w.move_out("d", &to);
    w.examine(&[("", "d")]);
    let engine = w.h.engine();
    engine.arm(Fault::MidStrip);
    w.h.drain(&engine);
    assert_eq!(w.deletes(), 0);
    assert_eq!(w.rows()[0].snapshot(), Some(CONTENT_LOCAL));
    let stripped = ["one.txt", "two.txt"].iter().filter(|n| konedrive_attrs(&to.join(n)).is_empty()).count();
    assert_eq!(stripped, 1, "one file stripped, one not");

    w.run();
    assert!(["one.txt", "two.txt"].iter().all(|n| konedrive_attrs(&to.join(n)).is_empty()));
    assert_eq!(std::fs::read(to.join("two.txt")).unwrap(), b"second");
    assert!(w.in_bin("D") && w.in_bin("P1") && w.in_bin("P2"));
    assert!(w.rows().is_empty());
}

/// A folder one of whose files cannot be downloaded goes nowhere: nothing is stripped or
/// deleted, and the row waits.
#[test]
fn a_folder_with_one_file_that_cannot_be_downloaded_stays() {
    let w = leaving(&[("D", None, "d/", b""), ("P1", Some("D"), "one.txt", b"first"), ("P2", Some("D"), "two.txt", b"second")]);
    w.serves("P2", b"other!");
    let to = w.beside("outside/d");
    w.move_out("d", &to);
    w.examine(&[("", "d")]);
    w.run();
    assert_eq!(w.deletes(), 0);
    assert_eq!(state(&to.join("one.txt")), Some(State::Hydrated), "what could be downloaded was");
    assert_ne!(state(&to.join("two.txt")), Some(State::Hydrated));
    assert!(!konedrive_attrs(&to.join("one.txt")).is_empty() && !konedrive_attrs(&to).is_empty(), "nothing stripped");
    assert!(w.helper.called("unmark_dir").is_empty());
    let row = &w.rows()[0];
    assert!(row.reason_text().as_deref().is_some_and(|r| r.starts_with(Reason::Download(None).key())), "{:?}", row.reason);
    assert_eq!(row.snapshot, None);
}

/// A folder moved back into the folder while its placeholders download is not
/// stripped, unmarked or deleted: the examination takes it on.
#[test]
fn a_folder_moved_back_during_its_download_is_left_alone() {
    struct MovesBack {
        from: PathBuf,
        to: PathBuf,
        inner: SourceFill,
    }
    #[async_trait]
    impl Filler for MovesBack {
        async fn fill(&self, file: File, shown: &Path, clearance: Option<&Clearance>) -> Result<(), FillError> {
            if self.from.exists() {
                std::fs::rename(&self.from, &self.to).unwrap();
            }
            self.inner.fill(file, shown, clearance).await
        }
    }
    let w = leaving(&[("D", None, "d/", b""), ("P1", Some("D"), "one.txt", b"first"), ("P2", Some("D"), "two.txt", b"second")]);
    let out = w.beside("outside/d");
    w.move_out("d", &out);
    w.examine(&[("", "d")]);
    w.fills_with(Arc::new(MovesBack { from: out.clone(), to: w.path("d"), inner: w.onedrive_fill() }));
    w.run();
    assert_eq!(w.deletes(), 0);
    assert!(w.helper.called("unmark_dir").is_empty(), "no directory in the folder is unmarked");
    for rel in ["d", "d/one.txt", "d/two.txt"] {
        assert!(konedrive_attrs(&w.path(rel)).iter().any(|a| a == XATTR_ITEM_ID), "{rel} keeps its item id");
    }
    let row = &w.rows()[0];
    assert_eq!((row.reason_text().as_deref(), row.snapshot), (Some(Reason::BackInside.key()), None));
}

/// A directory that only looks like a Trash (a `.Trash-<uid>` that is not at a mount's
/// top, with or without a `.trashinfo`) is anywhere else: downloaded first. So is a placeholder
/// in the real Trash that has another link: only its Trash name would go.
#[test]
fn a_lookalike_trash_and_a_linked_placeholder_are_downloaded_first() {
    let w = leaving(&[("L", None, "l.txt", b"looks like a trash"), ("H", None, "h.txt", b"linked")]);
    let fake = w.beside(&format!("outside/.Trash-{}", nix::unistd::geteuid().as_raw()));
    std::fs::create_dir_all(fake.join("info")).unwrap();
    std::fs::write(fake.join("info/l.txt.trashinfo"), "[Trash Info]\n").unwrap();
    let l = fake.join("files/l.txt");
    w.move_out("l.txt", &l);
    let h = w.to_trash("h.txt");
    std::fs::hard_link(&h, w.beside("outside/h-link.txt")).unwrap();
    w.examine(&[("", "l.txt"), ("", "h.txt")]);
    assert_eq!(w.rows().len(), 2);

    w.run();
    assert_eq!(std::fs::read(&l).unwrap(), b"looks like a trash");
    assert!(konedrive_attrs(&l).is_empty());
    assert_eq!(std::fs::read(&h).unwrap(), b"linked", "downloaded, not removed");
    assert!(w.in_bin("L") && w.in_bin("H"));
}

/// A fill that waits until the test lets it go.
struct HeldFill {
    begun: Arc<std::sync::atomic::AtomicBool>,
    go: Arc<tokio::sync::Notify>,
    inner: SourceFill,
}

#[async_trait]
impl Filler for HeldFill {
    async fn fill(&self, file: File, shown: &Path, clearance: Option<&Clearance>) -> Result<(), FillError> {
        self.begun.store(true, Ordering::SeqCst);
        self.go.notified().await;
        self.inner.fill(file, shown, clearance).await
    }
}

/// Waits, a few seconds at most, until `done` says so; whether it did.
fn eventually(done: impl Fn() -> bool) -> bool {
    let started = std::time::Instant::now();
    while !done() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    done()
}

/// While a row is in flight (a download held open here), a wake still re-marks what left:
/// the helper back is not held up behind the download.
#[test]
fn what_left_is_marked_again_while_other_rows_run() {
    let w = leaving(&[("P", None, "p.txt", b"slow"), ("Q", None, "q.txt", b"waits")]);
    let q = w.beside("outside/q.txt");
    w.move_out("p.txt", &w.beside("outside/p.txt"));
    w.move_out("q.txt", &q);
    w.examine(&[("", "p.txt"), ("", "q.txt")]);
    let (begun, go) = (Arc::new(std::sync::atomic::AtomicBool::new(false)), Arc::new(tokio::sync::Notify::new()));
    w.fills_with(Arc::new(HeldFill { begun: Arc::clone(&begun), go: Arc::clone(&go), inner: w.onedrive_fill() }));
    let engine = w.h.engine();
    let running = {
        let engine = Arc::clone(&engine);
        w.h.runtime.spawn(async move { engine.drain(&CancellationToken::new()).await })
    };
    let q_marks = || w.helper.called("mark_file").iter().filter(|p| **p == q).count();
    assert!(eventually(|| begun.load(Ordering::SeqCst)), "a download is in flight");
    assert_eq!(q_marks(), 1);
    engine.helper_back();
    assert!(eventually(|| q_marks() == 2), "marked again while the other row is still in flight");
    assert_eq!(w.deletes(), 0, "the download is still held");
    // Each fill that waits is let go: P's, then Q's.
    assert!(eventually(|| {
        go.notify_one();
        running.is_finished()
    }));
    w.h.runtime.block_on(running).unwrap();
    assert!(w.in_bin("P") && w.in_bin("Q"));
}

/// §5: a crash after the attributes came off and before the delete converges — the replay meets
/// `EPERM` (no item id any more), the marker says why, and the item is deleted.
#[test]
fn a_crash_between_the_strip_and_the_delete_converges() {
    let w = leaving(&[("P", None, "p.txt", b"content")]);
    let to = w.beside("outside/p.txt");
    w.move_out("p.txt", &to);
    w.examine(&[("", "p.txt")]);
    let engine = w.h.engine();
    engine.arm(Fault::AfterStrip);
    w.h.drain(&engine);
    assert_eq!(w.deletes(), 0);
    assert!(konedrive_attrs(&to).is_empty());
    assert_eq!(w.rows()[0].snapshot(), Some(CONTENT_LOCAL));
    assert_eq!(w.rows()[0].state, OutboxState::Running);

    w.run();
    assert_eq!(std::fs::read(&to).unwrap(), b"content");
    assert!(w.in_bin("P"));
    assert!(w.rows().is_empty());
}

/// §4.6: a folder moved out has every placeholder of its item downloaded where it went, the
/// attributes taken off every file and directory, every directory unmarked, and only then the
/// folder deleted in OneDrive, with one `DELETE`. Its directories stay, the user's own.
#[test]
fn a_folder_moved_out_is_downloaded_whole_then_deleted() {
    let w = leaving(&[
        ("D", None, "d/", b""),
        ("P1", Some("D"), "one.txt", b"first"),
        ("S", Some("D"), "sub/", b""),
        ("P2", Some("S"), "two.txt", b"second"),
        ("E", Some("D"), "empty/", b""),
    ]);
    let to = w.beside("outside/d");
    w.move_out("d", &to);
    w.examine(&[("", "d")]);
    assert_eq!(w.rows().iter().map(|r| (r.kind, r.item_id.clone())).collect::<Vec<_>>(), vec![(OutboxKind::MoveOut, Some("D".into()))]);

    w.run();
    assert_eq!(std::fs::read(to.join("one.txt")).unwrap(), b"first");
    assert_eq!(std::fs::read(to.join("sub/two.txt")).unwrap(), b"second");
    for path in [to.clone(), to.join("one.txt"), to.join("sub"), to.join("sub/two.txt"), to.join("empty")] {
        assert!(konedrive_attrs(&path).is_empty(), "{}", path.display());
    }
    let mut unmarked = w.helper.called("unmark_dir");
    unmarked.sort();
    assert_eq!(unmarked, vec![to.clone(), to.join("empty"), to.join("sub")]);
    assert!(w.in_bin("D") && w.in_bin("P1") && w.in_bin("P2"));
    assert_eq!(w.deletes(), 1, "one DELETE, for the folder");
    assert!(w.rows().is_empty());
}

/// §4.6, the Trash: nothing is downloaded. A placeholder is removed with its `.trashinfo`, and
/// so is a file whose free-up was cut short, which holds nothing whole either; a downloaded
/// file stays as the user's own; every item goes to OneDrive's recycle bin.
#[test]
fn the_trash_takes_placeholders_without_a_download() {
    let w = leaving(&[("T", None, "t.txt", b"placeholder"), ("H", None, "h.txt", b"downloaded"), ("F", None, "f.txt", b"half freed")]);
    // H was downloaded before it was trashed; F's free-up was cut short.
    w.hydrate("h.txt", b"downloaded");
    w.hydrate("f.txt", b"half freed");
    placeholder::write_state(&File::options().write(true).open(w.path("f.txt")).unwrap(), State::Dehydrating).unwrap();
    let trash = w.trash();
    let (t, h, f) = (w.to_trash("t.txt"), w.to_trash("h.txt"), w.to_trash("f.txt"));
    w.examine(&[("", "t.txt"), ("", "h.txt"), ("", "f.txt")]);
    assert_eq!(w.rows().len(), 3);

    w.run();
    assert!(!t.exists() && !trash.join("info/t.txt.trashinfo").exists(), "the placeholder left the Trash with its info");
    assert!(!f.exists() && !trash.join("info/f.txt.trashinfo").exists(), "so did the file that held nothing whole");
    assert_eq!(std::fs::read(&h).unwrap(), b"downloaded");
    assert!(konedrive_attrs(&h).is_empty());
    assert!(trash.join("info/h.txt.trashinfo").exists());
    assert!(w.in_bin("T") && w.in_bin("H") && w.in_bin("F"));
    assert_eq!(w.downloads(), 0);
    assert!(w.rows().is_empty());
}

/// §4.6, the Trash, for folders: nothing is downloaded into it. A folder of placeholders leaves
/// the Trash whole, with its `.trashinfo`; a folder holding a downloaded file keeps that file, as
/// the user's own, and loses its placeholders; a folder of placeholders put inside an entry of
/// the Trash goes, and the entry stays. OneDrive gets one `DELETE` for each folder and none for
/// what is inside (`docs/design/decisions.md`, "A folder delete is the whole folder, as on Windows").
#[test]
fn a_folder_in_the_trash_keeps_only_what_was_downloaded() {
    let w = leaving(&[
        ("E", None, "empty/", b""),
        ("E1", Some("E"), "one.txt", b"cloud only"),
        ("E2", Some("E"), "sub/", b""),
        ("E3", Some("E2"), "deep.txt", b"cloud only"),
        ("K", None, "kept/", b""),
        ("K1", Some("K"), "down.txt", b"downloaded"),
        ("K2", Some("K"), "cloud.txt", b"cloud only"),
        ("N", None, "nested/", b""),
        ("N1", Some("N"), "n.txt", b"cloud only"),
    ]);
    w.hydrate("kept/down.txt", b"downloaded");
    let trash = w.trash();
    for name in ["empty", "kept"] {
        w.to_trash(name);
    }
    // An entry of the user's own, and the third folder moved into it.
    std::fs::create_dir_all(trash.join("files/older")).unwrap();
    std::fs::write(trash.join("info/older.trashinfo"), "[Trash Info]\n").unwrap();
    w.move_out("nested", &trash.join("files/older/nested"));
    w.examine(&[("", "empty"), ("", "kept"), ("", "nested")]);
    assert_eq!(w.rows().len(), 3);

    w.run();
    assert!(!trash.join("files/empty").exists() && !trash.join("info/empty.trashinfo").exists());
    let kept = trash.join("files/kept");
    assert_eq!(std::fs::read(kept.join("down.txt")).unwrap(), b"downloaded");
    assert!(!kept.join("cloud.txt").exists(), "the placeholder left the Trash");
    assert!(konedrive_attrs(&kept).is_empty() && konedrive_attrs(&kept.join("down.txt")).is_empty());
    assert!(trash.join("info/kept.trashinfo").exists());
    assert!(!trash.join("files/older/nested").exists(), "left empty, it went");
    assert!(trash.join("files/older").exists() && trash.join("info/older.trashinfo").exists(), "the user's own entry stays");
    assert!(["E", "E1", "E3", "K", "K1", "K2", "N", "N1"].iter().all(|id| w.in_bin(id)));
    assert_eq!((w.deletes(), w.downloads()), (3, 0), "one DELETE for each folder, and nothing downloaded");
    assert!(w.rows().is_empty());
}

/// A folder deleted here outright sends OneDrive one `DELETE` of the whole folder and none for
/// what is inside, whatever its size: as Windows deletes a folder.
#[test]
fn a_folder_delete_sends_one_delete_and_none_for_what_is_inside() {
    let ids: Vec<String> = (0..20).map(|n| format!("P{n}")).collect();
    let names: Vec<String> = (0..20).map(|n| format!("{n}.txt")).collect();
    let mut items: Vec<(&str, Option<&str>, &str, &[u8])> = vec![("D", None, "d/", b"")];
    items.extend((0..20).map(|n| (ids[n].as_str(), Some("D"), names[n].as_str(), b"x" as &[u8])));
    let w = leaving(&items);
    std::fs::remove_dir_all(w.path("d")).unwrap();
    w.examine(&[("", "d")]);
    assert_eq!(w.rows().len(), 1);
    // 21 items out of 21 trips the mass-delete guard; confirming it is a
    // separate mechanism (write design §4.5).
    w.store.call_blocking(move |s| s.outbox_release_held()).unwrap();
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.rows());
    assert_eq!(w.deletes(), 1, "one DELETE only, for the folder");
    assert!(w.in_bin("D"));
}

/// `move-out` rows dropped with no worker to finish them (`RestoreDeletes`, a switch to
/// read-only, a Forget, a Remove) leave nothing outside that would read as zeros: a placeholder
/// goes, alone or in a folder; a downloaded file stays, stripped; the folder is stripped and
/// unmarked; and each item forgets its local object, so that it is placed again rather than
/// taken for a delete. Nothing is deleted in OneDrive.
#[test]
fn dropped_move_outs_leave_no_placeholder_outside() {
    let w = leaving(&[
        ("D", None, "d/", b""),
        ("P1", Some("D"), "cloud.txt", b"cloud"),
        ("P2", Some("D"), "down.txt", b"down"),
        ("P", None, "p.txt", b"cloud"),
        ("Q", None, "q.txt", b"down"),
    ]);
    w.hydrate("d/down.txt", b"down");
    w.hydrate("q.txt", b"down");
    let (d, p, q) = (w.beside("outside/d"), w.beside("outside/p.txt"), w.beside("outside/q.txt"));
    w.move_out("d", &d);
    w.move_out("p.txt", &p);
    w.move_out("q.txt", &q);
    w.examine(&[("", "d"), ("", "p.txt"), ("", "q.txt")]);
    assert_eq!(w.rows().len(), 3);
    assert!(w.store.call_blocking(move |s| s.local_handle("P")).unwrap().is_some());

    let dropped = w.store.call_blocking(crate::upload::move_out::drop_rows).unwrap();
    w.tidy(&dropped);
    assert!(!p.exists() && !d.join("cloud.txt").exists(), "the placeholders outside went");
    assert_eq!(std::fs::read(&q).unwrap(), b"down");
    assert_eq!(std::fs::read(d.join("down.txt")).unwrap(), b"down");
    assert!(konedrive_attrs(&q).is_empty() && konedrive_attrs(&d).is_empty() && konedrive_attrs(&d.join("down.txt")).is_empty());
    assert_eq!(w.helper.called("unmark_dir"), vec![d.clone()]);
    assert!(w.rows().is_empty());
    for id in ["P", "D", "P1"] {
        assert!(w.store.call_blocking(move |s| s.local_handle(id)).unwrap().is_none(), "{id} is placed again, not taken for a delete");
    }
    assert_eq!((w.deletes(), w.downloads()), (0, 0));
}

/// `docs/design/writes.md` §8.3: a placeholder moved into another account's folder, which
/// turns read-only before this account's move out has run. That folder's read-only reconcile, whose
/// tree does not know the id, sets the object aside alive rather than removing it; the move out
/// then finds it by its handle and downloads it where it is. The file ends up on disk with its
/// content, and not only in OneDrive's recycle bin.
#[test]
fn a_placeholder_moved_into_a_read_only_account_ends_up_on_disk() {
    let w = leaving(&[("P", None, "p.txt", b"the content")]);
    // Account B's folder, placed from its own listing while it was read-write.
    let b = w.another_folder("B");
    xattr::set(&b, XATTR_ROOT, b"b-root").unwrap();
    let b_root = SyncRoot { path: b.clone(), root_id: "b-root".into() };
    let b_store = Store::new(TreeStore::in_memory().unwrap());
    let b_items = vec![Change::Root(row("RB", None, "", Kind::Folder, b"")), Change::Upsert(row("B1", Some("RB"), "b.txt", Kind::File, b"b"))];
    let a_store = w.store.clone();
    let claimed: crate::remote::materialize::Claimed = Arc::new(move |id| { let id = id.to_owned(); a_store.call_blocking(move |s| Ok(s.get(Table::Items, &id)?.is_some())).unwrap() });
    let reconcile_b = |locked: bool| {
        Materializer {
            disk: Disk::open(&b_root, locked).unwrap(),
            store: b_store.clone(),
            link: None,
            runtime: w.h.runtime.handle().clone(),
            locks: InodeLocks::new(),
            root_item_id: "RB".into(),
            rescue_into: w.beside("rescued-b/now"),
            cancel: CancellationToken::new(),
            rw: None,
            claimed: Some(Arc::clone(&claimed)),
        }
        .apply(Scope::Full)
        .unwrap()
    };
    b_store
        .call_blocking(move |s| {
            s.begin_staging(konedrive_tree::NewTree::Whole)?;
            s.stage(&b_items)
        })
        .unwrap();
    reconcile_b(false);
    b_store.call_blocking(move |s| s.commit_staging("b-1")).unwrap();

    // The user moves A's placeholder into B; A's examination sees it leave, and its move out waits.
    let in_b = b.join("p.txt");
    w.move_out("p.txt", &in_b);
    w.examine(&[("", "p.txt")]);
    assert_eq!(w.rows()[0].kind, OutboxKind::MoveOut);

    // B is read-only now: its Full reconcile does not know P, and A claims it.
    b_store.call_blocking(move |s| s.begin_staging(konedrive_tree::NewTree::Delta)).unwrap();
    let applied = reconcile_b(true);
    assert!(!in_b.exists(), "out of B's folder");
    let aside = applied.on_disk.rescued.iter().find(|r| r.original == Path::new("p.txt")).map(|r| r.rescued.clone()).expect("set aside");
    assert_eq!(state(&aside), Some(State::OnlineOnly), "alive, attributes and all");
    assert_eq!(w.deletes(), 0);

    // A's move out runs: the helper finds the object by its handle, wherever it is.
    w.run();
    assert_eq!(std::fs::read(&aside).unwrap(), b"the content", "on disk, with its content");
    assert!(konedrive_attrs(&aside).is_empty());
    assert!(w.in_bin("P"));
    assert!(w.rows().is_empty());
}

/// A moved-out object gone from inside another account's folder, where it was
/// last proved to be, may have been removed there as none of that account's: nothing is deleted
/// in OneDrive. The row goes, and the item forgets its local object, so that it is placed again
/// here.
#[test]
fn a_move_out_gone_inside_another_accounts_folder_deletes_nothing() {
    let w = leaving(&[("P", None, "p.txt", b"p")]);
    let in_b = w.another_folder("B").join("p.txt");
    w.move_out("p.txt", &in_b);
    w.examine(&[("", "p.txt")]);
    assert_eq!(w.rows()[0].target_name.as_deref(), in_b.to_str());
    std::fs::remove_file(&in_b).unwrap();
    for _ in 0..2 {
        w.run();
        w.due_now();
    }
    assert_eq!(w.deletes(), 0, "nothing is deleted in OneDrive");
    assert!(w.rows().is_empty());
    assert!(w.store.call_blocking(move |s| s.local_handle("P")).unwrap().is_none(), "placed again by the next reconcile");
    assert!(w.store.call_blocking(move |s| s.get(Table::Items, "P")).unwrap().is_some());
}

/// A downloaded file moved into another account's read-write folder, whose examination
/// strips it and uploads it as its own before this account's move out ran. The move out meets
/// `EPERM` — never "gone" — but the object stands where it was last proved to be, with the same
/// handle: the row goes without a delete, and the item comes back here. Duplicated, nothing lost,
/// and no row waits for ever. (Stripped anywhere else, the row waits:
/// `an_object_that_cannot_be_reached_is_never_taken_for_gone`.)
#[test]
fn a_file_another_account_took_for_its_own_is_kept_here_too() {
    let w = leaving(&[("P", None, "p.txt", b"down")]);
    w.hydrate("p.txt", b"down");
    let in_b = w.another_folder("B").join("p.txt");
    w.move_out("p.txt", &in_b);
    w.examine(&[("", "p.txt")]);
    placeholder::strip_konedrive_xattrs(&File::open(&in_b).unwrap()).unwrap();
    w.run();
    assert_eq!(w.deletes(), 0, "nothing is deleted in OneDrive");
    assert!(w.rows().is_empty(), "no row waits for ever: {:?}", w.rows());
    assert!(w.store.call_blocking(move |s| s.local_handle("P")).unwrap().is_none());
    assert_eq!(std::fs::read(&in_b).unwrap(), b"down");
}

/// A placeholder that left a moved-out folder again before the folder's row ran is made local
/// where it went too, before the folder — and it with it — leaves OneDrive.
#[test]
fn what_left_a_moved_out_folder_since_is_downloaded_where_it_went() {
    let w = leaving(&[("D", None, "d/", b""), ("P", Some("D"), "p.txt", b"inside"), ("Q", Some("D"), "q.txt", b"went on")]);
    let to = w.beside("outside/d");
    w.move_out("d", &to);
    w.examine(&[("", "d")]);
    let q_now = w.beside("outside/q.txt");
    std::fs::rename(to.join("q.txt"), &q_now).unwrap();

    w.run();
    assert_eq!(std::fs::read(to.join("p.txt")).unwrap(), b"inside");
    assert_eq!(std::fs::read(&q_now).unwrap(), b"went on");
    assert!(konedrive_attrs(&q_now).is_empty());
    assert!(w.in_bin("D") && w.in_bin("Q"));
}

/// §4.6: an object back inside the folder before its row ran is the examination's: nothing is
/// downloaded, stripped or deleted, whether or not the row had begun (its marker stays).
#[test]
fn an_object_back_in_the_folder_is_left_to_the_examination() {
    for begun in [false, true] {
        let w = leaving(&[("P", None, "p.txt", b"p")]);
        w.move_out("p.txt", &w.beside("outside/p.txt"));
        w.examine(&[("", "p.txt")]);
        let seq = w.rows()[0].seq;
        if begun {
            w.store.call_blocking(move |s| s.outbox_set_snapshot(seq, Some(CONTENT_LOCAL))).unwrap();
        }
        std::fs::rename(w.beside("outside/p.txt"), w.path("back.txt")).unwrap();
        w.run();
        assert_eq!((w.deletes(), w.downloads()), (0, 0), "begun: {begun}");
        assert_eq!(state(&w.path("back.txt")), Some(State::OnlineOnly), "begun: {begun}");
        assert_eq!(w.rows()[0].reason_text().as_deref(), Some(Reason::BackInside.key()), "begun: {begun}");
        assert_eq!(w.rows()[0].snapshot(), begun.then_some(CONTENT_LOCAL), "begun: {begun}");
    }
}

/// §4.6, §5: what left is marked again before anything else, whatever the rows' states — here a
/// paused worker, which sends nothing — once per helper connection, and again after the helper
/// comes back; and the router is told whose the ids are.
#[test]
fn what_left_is_marked_again_first_even_while_paused() {
    let w = leaving(&[("D", None, "d/", b""), ("P", Some("D"), "p.txt", b"p"), ("Q", None, "q.txt", b"q")]);
    let d = w.beside("outside/d");
    let q = w.beside("outside/q.txt");
    w.move_out("d", &d);
    w.move_out("q.txt", &q);
    w.examine(&[("", "d"), ("", "q.txt")]);
    let routed: Arc<Mutex<Vec<std::collections::HashSet<String>>>> = Arc::default();
    {
        let mut moved_out = w.h.moved_out.lock().unwrap();
        let routed = Arc::clone(&routed);
        moved_out.as_mut().unwrap().route = Some(Arc::new(move |ids| routed.lock().unwrap().push(ids)));
    }
    let engine = w.h.engine();
    pause(&engine);
    w.h.drain(&engine);
    assert_eq!(w.deletes(), 0, "paused");
    assert_eq!(w.helper.called("mark_file"), vec![q.clone()]);
    assert_eq!(w.helper.called("mark_dir"), vec![d.clone()]);
    let ids = routed.lock().unwrap().last().cloned().unwrap();
    assert_eq!(ids, ["D", "P", "Q"].into_iter().map(String::from).collect());

    w.h.drain(&engine);
    assert_eq!(w.helper.called("mark_file").len(), 1, "once per helper connection");
    engine.helper_back();
    w.h.drain(&engine);
    assert_eq!(w.helper.called("mark_file").len(), 2, "again once the helper is back");
    assert_eq!(routed.lock().unwrap().len(), 1, "the router is told only of a change");
}

/// The liveness the daemon's examination asks: `ESTALE` is gone, a descriptor says where,
/// and `EPERM` — never gone — decides nothing.
#[test]
fn the_helpers_answer_is_read_as_the_examination_needs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("x");
    std::fs::write(&path, b"").unwrap();
    let fd: OwnedFd = File::open(&path).unwrap().into();
    assert_eq!(answered(Ok(fd)).unwrap(), Whereabouts::At(path));
    assert_eq!(answered(Err(HelperError::Refused(libc::ESTALE))).unwrap(), Whereabouts::Gone);
    assert_eq!(answered(Err(HelperError::Refused(libc::EPERM))).unwrap_err().raw_os_error(), Some(libc::EPERM));
    assert!(answered(Err(HelperError::NotRunning)).is_err());
}

#[test]
fn a_trash_is_known_by_its_place() {
    let home = Path::new("/home/u/.local/share/Trash");
    let mounts = |p: &Path| p == Path::new("/mnt/d");
    let entry = trash_of(Path::new("/home/u/.local/share/Trash/files/a.txt"), Some(home), 1000, &mounts).unwrap();
    assert_eq!(entry.top, Path::new("/home/u/.local/share/Trash/files/a.txt"));
    assert_eq!(entry.info, Path::new("/home/u/.local/share/Trash/info/a.txt.trashinfo"));
    let inside = trash_of(Path::new("/mnt/d/.Trash-1000/files/dir/deep/x"), None, 1000, &mounts).unwrap();
    assert_eq!(inside.top, Path::new("/mnt/d/.Trash-1000/files/dir"));
    assert_eq!(inside.info, Path::new("/mnt/d/.Trash-1000/info/dir.trashinfo"));
    let shared = trash_of(Path::new("/mnt/d/.Trash/1000/files/y"), None, 1000, &mounts).unwrap();
    assert_eq!(shared.shared.as_deref(), Some(Path::new("/mnt/d/.Trash")));
    assert!(trash_of(Path::new("/mnt/d/.Trash-1001/files/y"), None, 1000, &mounts).is_none(), "another user's");
    assert!(trash_of(Path::new("/mnt/d/x/.Trash-1000/files/y"), None, 1000, &mounts).is_none(), "not at the mount's top");
    assert!(trash_of(Path::new("/mnt/e/.Trash-1000/files/y"), None, 1000, &mounts).is_none(), "not a mount");
    assert!(trash_of(Path::new("/home/u/Trash/files/y"), Some(home), 1000, &mounts).is_none());
    assert!(trash_of(Path::new("/home/u/.local/share/Trash/files"), Some(home), 1000, &mounts).is_none(), "the Trash itself");
}

/// A fill that runs one blocking section as the real fill runs its own (`hydration::source`),
/// under the lock it was started under. The section waits until the fill is dropped, then
/// goes on for a while before it ends.
struct SectionFill {
    begun: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    ended: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl Filler for SectionFill {
    async fn fill(&self, _file: File, _shown: &Path, _clearance: Option<&Clearance>) -> Result<(), FillError> {
        let hold = crate::folder::locks::hold_in_force();
        let begun = self.begun.lock().unwrap().take().expect("one fill");
        let ended = Arc::clone(&self.ended);
        // Dropped with this future: the section learns that its fill is gone.
        let (_alive, dropped) = std::sync::mpsc::channel::<()>();
        let section = tokio::task::spawn_blocking(move || {
            let _hold = hold;
            begun.send(()).unwrap();
            let _ = dropped.recv();
            std::thread::sleep(std::time::Duration::from_millis(300));
            ended.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        section.await.unwrap();
        Ok(())
    }
}

/// A stop asked while a move-out row's fill has a section under way returns only once the
/// section has ended: what comes after the stop (the rows dropped, the tidying) finds nothing
/// of the worker at work, and the file's lock free.
#[test]
fn a_stop_waits_for_the_section_of_a_move_outs_fill() {
    let w = leaving(&[("P", None, "p.txt", b"the content")]);
    let to = w.beside("outside/p.txt");
    w.move_out("p.txt", &to);
    w.examine(&[("", "p.txt")]);
    let (begun, has_begun) = std::sync::mpsc::channel();
    let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
    w.fills_with(Arc::new(SectionFill { begun: Mutex::new(Some(begun)), ended: Arc::clone(&ended) }));
    let config = w.h.config();
    let locks = config.locks.clone();
    let worker = OutboxWorker::new(config);
    w.h.runtime.block_on(async {
        worker.start();
        tokio::task::spawn_blocking(move || has_begun.recv().unwrap()).await.unwrap();
        worker.stop().await;
    });
    assert!(ended.load(std::sync::atomic::Ordering::SeqCst), "the stop returned while the fill's section was running");
    let key = crate::folder::locks::InodeKey::of(&File::open(&to).unwrap()).unwrap();
    assert!(locks.try_lock(key).is_some(), "the file's lock is free after the stop");
}
