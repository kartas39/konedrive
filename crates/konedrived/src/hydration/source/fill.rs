//! Filling a placeholder in place from a content source: the state the file is found in
//! and left in, the clearing of its ignore mark, the download, the commit, and the
//! roll-back of a fill that failed.

use std::fs::File;
use std::os::fd::OwnedFd;

use konedrive_fs::placeholder::{
    read_item_id, read_state, remove_progress, set_mtime, stamp_matches, with_owner_write, write_ctag, write_stamp,
    write_state, State, XATTR_STATE,
};

use crate::helper::{Clearance, HelperLink, NotCleared};
use crate::hydration::demote::{demote, drop_checkpoint, usable_checkpoint, Demoted, FileTimes, Held, Keep, Shape};

use super::download::{download, Downloaded};
use super::guards::{errno_of, Tuning};
use super::parts;
use super::target::Target;
use super::{ContentSource, Split};
use konedrive_fs::placeholder::Progress;

/// Why a fill did not happen, or did not finish.
#[derive(Debug)]
pub enum FillError {
    /// It ran and failed; the opener is answered with this errno, and the
    /// file was rolled back (see [`roll_back`]). Every value is in
    /// `konedrive_proto::ACCEPTED_DENY_ERRNOS`: any other makes the helper's
    /// answer to the kernel fail, and the suspended `open()` hang.
    Errno(i32),
    /// It never started: the file may carry an ignore mark
    /// that could not be cleared, and a fill that fails empties the file.
    /// Nothing was fetched, and the state is back to what it was found in.
    NotCleared(NotCleared),
}

impl FillError {
    /// What a suspended open is answered with.
    pub fn errno(&self) -> i32 {
        match self {
            FillError::Errno(errno) => *errno,
            FillError::NotCleared(_) => libc::EIO,
        }
    }
}

/// What an intercepted open's hydration request does once it holds the
/// per-inode lock: **look again**, and fill only a
/// file that still needs it.
///
/// A request can wait a long time before it gets here — for a slot of its
/// account's transfer pool behind other downloads, or in the helper for credit — and the
/// file it names can have been filled meanwhile: by `Hydrate()`, or because a
/// `Dehydrate` that the open itself made fail (its suspended descriptor
/// refuses the lease) rolled the file back to `hydrated`. Filling it again
/// without looking was The re-fill wrote `hydrating`
/// over a hydrated file and fetched; a failed fetch rolled back — demoted and
/// punched — a file that an opener in between had found `hydrated` and had
/// the helper ignore-mark, and the next reader got 65 536 zero bytes without
/// being intercepted at all (measured on Btrfs, ext4 and XFS, nothing
/// injected). A re-fetch that succeeded overwrote any edit made in place
/// since, which is data loss of the other kind.
///
/// So a file that reads `hydrated` is answered 0 and not touched, whatever
/// its stamp says. With a matching stamp it is simply there; with a stamp
/// that does not match it was edited locally, and that edit is the only copy
/// (`docs/design/hydration.md` §6.1 step 0); with none it was labelled by something other than this daemon — the
/// case `Hydrate()` repairs on request, and never something to do
/// behind an opener's back, since the helper lets every opener of a
/// `hydrated` file through anyway. The helper reads the state again itself
/// before it lets the opener through (§5.1 step 5).
pub async fn answer_request(
    fd: OwnedFd,
    source: &dyn ContentSource,
    link: Option<&HelperLink>,
) -> Answered {
    let target = Target::new(File::from(fd));
    let found = target
        .alone(|file| {
            let state = read_state(file);
            let edited = matches!(state, Ok(Some(State::Hydrated))) && !matches!(stamp_matches(file), Ok(true));
            (state, edited)
        })
        .await;
    match found.0 {
        Ok(Some(State::Hydrated)) => {
            let edited = found.1;
            tracing::info!(
                "a hydration request found its file already hydrated — filled while the request \
                 waited{} — and answers it without touching the file",
                if edited { ", and changed since (or never stamped): its content is kept" } else { "" }
            );
            Answered::AlreadyThere
        }
        Ok(Some(_)) => {
            let clearance = link.map(|link| Clearance::Link(link.clone()));
            match Fill::new(source).clearance(clearance.as_ref()).run_on(target).await {
                Ok(()) => Answered::Filled,
                Err(e) => Answered::Failed(e),
            }
        }
        Ok(None) => {
            tracing::error!(
                "a hydration request names a file with no konedrive state; it is not ours to fill"
            );
            Answered::NotOurs
        }
        Err(e) => {
            tracing::error!("cannot read the state of a file a hydration request names: {e}");
            Answered::NotOurs
        }
    }
}

/// What [`answer_request`] did, which decides both what the opener is
/// answered ([`errno`](Self::errno)) and what the activity log records:
/// only a fill that ran is an event.
#[derive(Debug)]
pub enum Answered {
    /// Filled while the request waited, or edited here: left as it is.
    AlreadyThere,
    Filled,
    /// The fill ran, or could not start, and the file is as it was.
    Failed(FillError),
    /// No konedrive state, or none that can be read: not ours to fill.
    NotOurs,
}

impl Answered {
    /// What the suspended open is answered with; 0 lets it through.
    pub fn errno(&self) -> i32 {
        match self {
            Answered::AlreadyThere | Answered::Filled => 0,
            Answered::Failed(e) => e.errno(),
            Answered::NotOurs => libc::EIO,
        }
    }
}

/// Fills the placeholder `fd` is open on, in place, clearing the file's
/// ignore mark first when it could be carrying one.
///
/// # Can this file carry a mark placed after the last clear?
///
/// That is the question every punch has to answer, and a fill can punch:
/// [`roll_back`] empties the file
/// when the fill fails. The helper places an ignore mark only on a file it
/// has read `hydrated` — and reads again, after placing it —
/// so which state the fill starts from decides the answer:
///
/// - `online-only`: no. An open of it raised an event, so it carried no
///   mark then, and none can be placed once this fill has written
///   `hydrating`. (A stale mark on an `online-only` file already reads
///   zeros; filling the file repairs that, and failing leaves it as it was.)
/// - `hydrated` (only `Hydrate()` fills one, when its stamp is missing)
///   and `dehydrating` (a `Dehydrate` cancelled between its state write and
///   its `ClearIgnore`): yes. `hydrating`, which a panicked or
///   crashed fill of such a file leaves: possibly.
///
/// For those, `hydrating` is made durable first and then the way is cleared
/// by local rule ([`Clearance`]) — the helper asked to
/// `ClearIgnore`, in the order §8 uses, so that an opener's mark placed in
/// between is found and taken off again by the helper's own re-read; or, with
/// no link, the helper's socket looked at — before the first byte is
/// fetched. If the way is not cleared, nothing is fetched, the state it was
/// found in is put back, and the answer is [`FillError::NotCleared`].
///
/// Which of the two it is, is decided here and nowhere else, from the state
/// this fill reads itself. `clearance` is what the caller has to clear the
/// way with, and `None` is "nothing": an `online-only` file — the case above
/// that needs nothing — is filled all the same, and any other is refused
/// [`FillError::NotCleared`] before it is touched. It is never `None` on the
/// strength of the folder's mode: a folder without interception is cleared
/// like any other.
pub async fn hydrate_with(
    fd: OwnedFd,
    source: &dyn ContentSource,
    clearance: Option<&Clearance>,
) -> Result<(), FillError> {
    Fill::new(source).clearance(clearance).run(fd).await
}

/// [`hydrate_with`], downloading the file in parallel parts ([`parts`]): a large pinned
/// file. Everything else about the fill — the state, the clearance, the
/// checkpoint, the roll-back and the commit — is the same.
pub async fn hydrate_in_parts(
    fd: OwnedFd,
    source: &dyn ContentSource,
    clearance: Option<&Clearance>,
    split: &Split,
) -> Result<(), FillError> {
    Fill::new(source).clearance(clearance).in_parts(split).run(fd).await
}

/// One fill, as it is asked for: [`hydrate_with`] and [`hydrate_in_parts`] are its two
/// spellings outside this module.
pub(super) struct Fill<'a> {
    source: &'a dyn ContentSource,
    clearance: Option<&'a Clearance>,
    split: Option<&'a Split>,
    tuning: Tuning,
    before_commit: BeforeCommit,
}

impl<'a> Fill<'a> {
    pub(super) fn new(source: &'a dyn ContentSource) -> Self {
        Self { source, clearance: None, split: None, tuning: Tuning::default(), before_commit: BeforeCommit::default() }
    }

    pub(super) fn clearance(mut self, clearance: Option<&'a Clearance>) -> Self {
        self.clearance = clearance;
        self
    }

    pub(super) fn in_parts(mut self, split: &'a Split) -> Self {
        self.split = Some(split);
        self
    }

    /// With other numbers than the daemon's (tests: a checkpoint within a small file, no
    /// pause after a break).
    #[cfg(test)]
    pub(super) fn tuning(mut self, tuning: Tuning) -> Self {
        self.tuning = tuning;
        self
    }

    pub(super) async fn run(self, fd: OwnedFd) -> Result<(), FillError> {
        self.run_on(Target::new(File::from(fd))).await
    }

    /// The fill, in blocking sections ([`Target`]), cut where the calls waited for
    /// something else before: [`begin`]; the clearing, and [`put_back`] if it fails; where
    /// to continue from ([`resume_point`]); the download, a section for each read of the
    /// stream; [`commit`], or [`roll_back`].
    async fn run_on(self, target: Target) -> Result<(), FillError> {
        let has_clearance = self.clearance.is_some();
        let Begun { item_id, shape, found, found_raw } = target.alone(move |file| begin(file, has_clearance)).await?;
        let clearance = match found {
            Some(State::OnlineOnly) => None,
            _ => self.clearance,
        };
        if let Some(clearance) = clearance {
            if let Err(e) = clearance.clear(target.file()).await {
                tracing::error!(
                    "{item_id}: the way was not cleared for filling a file found {found:?} \
                     ({e}); not filling it, since a failed fill would empty a file that may \
                     still be ignored"
                );
                target.alone(move |file| put_back(file, found_raw.as_deref())).await;
                return Err(FillError::NotCleared(e));
            }
        }

        let original_size = shape.size.unwrap_or(0);
        let filled = async {
            let resume = target.alone(move |file| resume_point(file, original_size)).await?;
            let downloaded = match self.split {
                Some(split) => parts::download(&target, &item_id, original_size, self.source, resume, split, &self.tuning).await?,
                None => download(&target, &item_id, original_size, self.source, resume, &self.tuning).await?,
            };
            let before_commit = self.before_commit;
            target.alone(move |file| commit(file, &downloaded, before_commit)).await
        };
        if let Err(errno) = filled.await {
            target.alone(move |file| roll_back(file, shape)).await;
            return Err(FillError::Errno(errno));
        }
        Ok(())
    }
}

/// What a fill found on its file before it wrote `hydrating` over it.
struct Begun {
    item_id: String,
    /// The placeholder's own size and times — the cloud's — which a roll-back
    /// puts back over what the fill's writes made of them.
    shape: Shape,
    /// The state as it was on the file, for putting back exactly.
    found: Option<State>,
    found_raw: Option<Vec<u8>>,
}

/// The fill's first section: what the file is, whether it may be filled with
/// what there is to clear its way with, and `state=hydrating`, durable.
fn begin(file: &File, has_clearance: bool) -> Result<Begun, FillError> {
    let meta = file.metadata().map_err(|e| FillError::Errno(errno_of(&e)))?;
    let times = FileTimes::of(file).map_err(|e| FillError::Errno(errno_of(&e)))?;
    let Ok(Some(item_id)) = read_item_id(file) else {
        return Err(FillError::Errno(libc::EIO));
    };
    // The state as it is on the file, for putting back exactly; one that
    // does not parse is not "no state", and is not `online-only` either.
    let found_raw = xattr::FileExt::get_xattr(file, XATTR_STATE).map_err(|e| FillError::Errno(errno_of(&e)))?;
    let found: Option<State> = found_raw.as_deref().and_then(|raw| String::from_utf8_lossy(raw).parse().ok());
    // Only an `online-only` file needs no clearing. With no way to clear,
    // any other is not filled: a failed fill would empty a file that may
    // still be ignored.
    //
    // The ordinary way here is a helper that is away (an intercepted
    // folder with no link): the caller says so where it used to, so
    // this is not an error to write at every attempt.
    if !matches!(found, Some(State::OnlineOnly)) && !has_clearance {
        tracing::debug!("{item_id}: found {found:?}, and nothing to clear its ignore mark with; not filled");
        return Err(FillError::NotCleared(NotCleared::NoWay));
    }
    // §6.1 step 1: `state=hydrating`, `fsync`. The marker has to be durable
    // before the first byte lands, or a power loss leaves a file that looks
    // `online-only` while holding allocated blocks full of partial content,
    // and §9 startup recovery has nothing to find it by.
    write_state(file, State::Hydrating).map_err(|e| FillError::Errno(errno_of(&e)))?;
    file.sync_all().map_err(|e| FillError::Errno(errno_of(&e)))?;
    Ok(Begun { item_id, shape: Shape { size: Some(meta.len()), times }, found, found_raw })
}

/// Undoes the `hydrating` a fill wrote before it had touched anything else:
/// the state attribute gets back the bytes it was found with, whether they
/// parse as a state or not, and is taken off only if there was none.
fn put_back(file: &File, found: Option<&[u8]>) {
    let restored = match found {
        // As `write_state` writes it: a file may not be writable by its owner.
        Some(raw) => with_owner_write(file, || xattr::FileExt::set_xattr(file, XATTR_STATE, raw)),
        None => xattr::FileExt::remove_xattr(file, XATTR_STATE),
    };
    if let Err(e) = restored {
        tracing::error!(
            "cannot put the state back to {:?} ({e}); the file is left `hydrating`, which \
             the next open fills again",
            found.map(String::from_utf8_lossy)
        );
    }
}

/// Undoes a failed fill ([`demote`]): the file is a placeholder again, with the size and
/// the times it had before the fill, and no content — or, when the download made a
/// checkpoint durable, the checkpointed prefix and its `user.konedrive.progress`, which the
/// next fill continues from. The prefix is not trusted by being kept: the fill that
/// continues from it reads it back into the hash, and the whole file is checked against
/// its quickXorHash before it is ever `hydrated`.
///
/// **Only a file still `hydrating` is touched.** That is the state this fill wrote, and
/// under the per-inode lock nothing else in this daemon changes it. The fill itself writes
/// one more — `hydrated`, its commit point — and only the final `fsync` can fail after it;
/// by then the content, its `fdatasync` and its stamp are all complete, so there is nothing
/// to undo. Anything else means something outside the daemon changed the state, and the
/// file is no longer this fill's to empty — least of all one that reads `hydrated`, which
/// the helper lets every opener through and may have ignore-marked. Emptying such a file
/// is the zeros case.
///
/// The punch is safe to make because of what the fill did before it fetched
/// ([`hydrate_with`]): it found the file `online-only`, or it cleared the way once
/// `hydrating` was durable — a file it could do neither for was refused — and nothing
/// places a mark on a file that reads `hydrating`.
///
/// A roll-back that fails itself leaves the file `hydrating`, which the next open fills
/// again and the next start resets.
fn roll_back(file: &File, shape: Shape) {
    match demote(file, Keep::Checkpoint, shape, Held::Fill) {
        Ok(Demoted::Done { kept: 0 }) => {}
        Ok(Demoted::Done { kept }) => tracing::info!("a failed hydration keeps its first {kept} bytes for the next attempt"),
        Ok(Demoted::Left(now)) => tracing::error!(
            "a failed fill found its file {now:?}, not `hydrating`: either its own last fsync failed \
             after the commit point, and the file is complete, or something outside the daemon \
             changed the state; leaving it exactly as it is rather than empty a file that is no \
             longer this fill's"
        ),
        Err(e) => tracing::error!(
            "cannot turn a failed hydration back into a placeholder ({e}); it is left `hydrating`, \
             which the next open or the next start takes from there"
        ),
    }
}

/// The checkpoint the download continues from, if there is one it can. One it cannot —
/// empty, past the end of the file, unreadable — is taken off before a single new byte is
/// written under it, durably: left in place, it would count bytes it knows nothing about,
/// and after a crash recovery could keep them as a checkpoint.
fn resume_point(file: &File, original_size: u64) -> Result<Option<Progress>, i32> {
    let resume = usable_checkpoint(file, original_size);
    if resume.is_none() && drop_checkpoint(file).map_err(|e| errno_of(&e))? {
        tracing::info!("dropped a download checkpoint that cannot be continued from");
        file.sync_all().map_err(|e| errno_of(&e))?;
    }
    Ok(resume)
}

/// Steps 4 and 5, in order: `state=hydrated`, the
/// commit point, is written last, after the size, the data, the cTag and the
/// stamp are durable.
///
/// `state=hydrated` is what makes the helper allow the open and ignore-mark
/// the inode, so every step that can still fail runs before it, never after
/// it has landed. The old order wrote it before `write_stamp` and `sync_all`,
/// so a full or failing disk produced a file marked `hydrated` holding
/// nothing but zeros while the fill reported `EIO` — and the helper answers
/// that by allowing, and ignore-marking, the very next open.
fn commit(file: &File, downloaded: &Downloaded, before_commit: BeforeCommit) -> Result<(), i32> {
    file.set_len(downloaded.size).map_err(|e| errno_of(&e))?;
    // An mtime the local filesystem cannot hold does **not** fail
    // the hydration. The property that outranks everything in this component
    // is "never serve zeros", and neither answer here serves zeros — the
    // file's content is complete and byte-for-byte correct either way — so
    // this is not a fail-closed decision at all. Denying would trade a
    // cosmetic metadata disagreement for the user not being able to read a
    // correct file. An mtime we cannot apply is a metadata problem, and it
    // belongs to the metadata sync path, exactly where put a
    // contradictory *size*.
    //
    // The ordering still matters, and it is what keeps this safe: `write_stamp`
    // below records the size and mtime the file *actually has* at that moment,
    // so after a failed `futimens` the stamp holds the local mtime rather than
    // the source's. `stamp_matches` therefore still holds, and §8 dehydration
    // still works on this file instead of refusing it as "modified locally".
    if let Err(e) = set_mtime(file, downloaded.mtime) {
        tracing::warn!(
            "cannot apply the mtime the source reported ({e}); hydrating anyway — the content is \
             complete and correct, and the stamp records the mtime the file actually has"
        );
    }
    file.sync_data().map_err(|e| errno_of(&e))?;
    if let Some(version) = &downloaded.version {
        write_ctag(file, &version.ctag).map_err(|e| errno_of(&e))?;
    }
    remove_progress(file).map_err(|e| errno_of(&e))?;
    write_stamp(file).map_err(|e| errno_of(&e))?;
    file.sync_all().map_err(|e| errno_of(&e))?;
    before_commit.fire()?;
    write_state(file, State::Hydrated).map_err(|e| errno_of(&e))?;
    file.sync_all().map_err(|e| errno_of(&e))?;
    Ok(())
}

/// What a test has happen at the last instant before `state=hydrated` is written: the one
/// failure after the data has landed that no unprivileged test can provoke on a real
/// filesystem. Nothing in every other build.
#[derive(Default)]
struct BeforeCommit(#[cfg(test)] Option<Box<dyn FnOnce() -> Option<i32> + Send>>);

impl BeforeCommit {
    fn fire(self) -> Result<(), i32> {
        #[cfg(test)]
        if let Some(errno) = self.0.and_then(|fault| fault()) {
            return Err(errno);
        }
        Ok(())
    }
}

#[cfg(test)]
impl<'a> Fill<'a> {
    /// `fault` runs right before the commit write; an errno it returns fails the commit
    /// there, as any other failure after the data would.
    pub(super) fn before_commit(mut self, fault: impl FnOnce() -> Option<i32> + Send + 'static) -> Self {
        self.before_commit = BeforeCommit(Some(Box::new(fault)));
        self
    }
}

#[cfg(test)]
mod tests;
