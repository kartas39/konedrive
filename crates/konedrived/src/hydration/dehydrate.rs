//! Freeing a downloaded file's space again (`docs/design/hydration.md` §8).
//!
//! # One descriptor, from the first open to the last write
//!
//! A free-up destroys a file's contents on purpose, so everything it decides
//! and everything it does must be about the same inode. The file is opened
//! **once**, and every step after that — reading the state, checking the
//! stamp, marking it `dehydrating`, handing the descriptor to the helper for
//! `ClearIgnore`, taking the write lease, emptying it — goes through that one
//! descriptor. A descriptor cannot be renamed out from under its holder: a
//! version that opened the path once per step emptied the file an editor's
//! save had put in the old one's place.
//!
//! The emptying itself is [`demote`], shared with a failed fill and with
//! startup recovery.

use std::fs::File;
use std::path::Path;

use konedrive_fs::lease::WriteLease;
use konedrive_fs::placeholder::{read_stamp, read_state, stamp_matches, write_state, State};

use crate::helper::{Clearance, HelperLink};
use crate::folder::root::{OpenError, SyncRoot};
use crate::helper::NotCleared;

use super::demote::{demote, Demoted, FileTimes, Held, Keep, Shape};

/// Why a file was not freed up.
#[derive(Debug, thiserror::Error)]
pub enum DehydrateError {
    #[error("not a OneDrive file")]
    NotManaged,
    #[error("the file is not downloaded")]
    NotHydrated,
    #[error("the file was modified locally")]
    ModifiedLocally,
    #[error("the file is in use")]
    InUse,
    /// A helper is running and this daemon has no link to it, so a mark its
    /// group may hold on the file cannot be cleared. Nothing
    /// was changed; try again once the link is up.
    #[error("the konedrive helper is running but not connected to this daemon")]
    HelperNotConnected,
    /// It could not be opened as a file of the folder ([`dehydrate`] only).
    #[error(transparent)]
    Open(#[from] OpenError),
    #[error("{0}")]
    Io(String),
}

fn io_error(e: impl std::fmt::Display) -> DehydrateError {
    DehydrateError::Io(e.to_string())
}

impl From<NotCleared> for DehydrateError {
    fn from(e: NotCleared) -> Self {
        match e {
            NotCleared::Unlinked => DehydrateError::HelperNotConnected,
            other => io_error(other),
        }
    }
}

/// The guard: only a clean, fully downloaded file may be emptied
/// (dehydration's step 1). It runs on the very descriptor the punch will
/// use, so what it decided about cannot be swapped for something else
/// afterwards.
pub fn check_dehydratable(file: &File) -> Result<(), DehydrateError> {
    match read_state(file).map_err(io_error)? {
        None => Err(DehydrateError::NotManaged),
        Some(State::Hydrated) => {
            if stamp_matches(file).map_err(io_error)? {
                Ok(())
            } else {
                Err(DehydrateError::ModifiedLocally)
            }
        }
        Some(_) => Err(DehydrateError::NotHydrated),
    }
}

/// Whether `file` is a zero-byte file as `create_placeholder` makes one:
/// `hydrated` from birth, with no stamp, since there is nothing to download.
/// Freeing it up has nothing to free, and succeeds without touching it
/// — it used to fail the stamp check and answer
/// `ModifiedLocally`, which told the user their edits would be lost. A file
/// that *became* empty here still carries the stamp of what it held, fails
/// that check, and is refused as modified.
fn nothing_to_free(file: &File) -> Result<bool, DehydrateError> {
    if read_state(file).map_err(io_error)? != Some(State::Hydrated) {
        return Ok(false);
    }
    let empty = file.metadata().map_err(io_error)?.len() == 0;
    Ok(empty && read_stamp(file).map_err(io_error)?.is_none())
}

/// Dehydration's (`docs/design/hydration.md` §8) steps 1–2, first half:
/// check, then publish `dehydrating` and make
/// it durable *before* anyone is told to stop intercepting this file.
///
/// # Why this cannot be moved after `ClearIgnore`
///
/// The order is the difference between failing loudly and failing silently.
/// With the mark cleared and the state still reading `hydrated`, an open
/// landing in that window reaches the helper's `handle_open`, matches
/// `Ok(Some(State::Hydrated))`, and is answered by **placing a fresh ignore
/// mark and allowing** — and then we punch. The file ends up empty *and*
/// permanently un-intercepted: zeros, forever, with no `ClearIgnore` failure
/// anywhere to notice. The lease does not cover that window; it is taken
/// afterwards.
///
/// With `dehydrating` already on disk the same open takes the `hydrate(...)`
/// arm instead: it waits, and re-hydrates after we finish, which is exactly
/// what §8's closing paragraph promises. The `fsync` is what makes the new
/// state survive a crash in the middle of the sequence, where startup
/// recovery (§4.4) then finds a `dehydrating` file and cleans it up.
/// # The barrier's own failure rolls back, like the two either side
///
/// If the `fsync` fails, the state write before it still happened — in page
/// cache, on a file that is otherwise exactly as hydrated as it was a moment
/// ago. Returning `Err` and leaving `dehydrating` behind would announce a
/// dehydration that never started, and the next startup recovery would empty
/// a perfectly good, fully hydrated file on the strength of it. So this
/// failure rolls the state back to `hydrated` exactly as a refused
/// `ClearIgnore` and a refused lease do.
fn mark_dehydrating(file: &File) -> Result<FileTimes, DehydrateError> {
    check_dehydratable(file)?;
    // Captured before anything touches the file: `fallocate` bumps the mtime
    // to now, and §4.2 requires an `online-only` file's mtime to still be the
    // remote `lastModifiedDateTime` that `create_placeholder` set.
    let times = FileTimes::of(file).map_err(io_error)?;
    write_state(file, State::Dehydrating).map_err(io_error)?;
    if let Err(e) = file.sync_all() {
        return Err(roll_back(file, io_error(e)));
    }
    Ok(times)
}

/// Undoes [`mark_dehydrating`] when the sequence stops before the punch
/// (steps 2 and 3 both say "roll back and report").
///
/// The rollback's own failure is reported, never swallowed: it leaves a file
/// stuck in `dehydrating` with its blocks intact, which startup recovery
/// (§4.4) will punch and turn into `online-only` — correct, but a silent
/// "free up space failed" that empties the file at the next start is not
/// something to discover from a log line that was never written.
fn roll_back(file: &File, cause: DehydrateError) -> DehydrateError {
    match write_state(file, State::Hydrated) {
        Ok(()) => cause,
        Err(e) => {
            tracing::error!("cannot roll the state back to hydrated after {cause}: {e}");
            DehydrateError::Io(format!(
                "{cause}, and rolling the state back to hydrated failed: {e}"
            ))
        }
    }
}

/// Dehydration's steps 3–5: take the lease, punch, put the mtime back, publish
/// `online-only`.
///
/// The caller must have cleared the helper's ignore mark first (invariant
/// M3) — this is private for that reason: an exported function
/// that punches with no helper involvement puts the project's one silent,
/// unrecoverable failure behind nothing but a doc comment.
///
/// Only what stops the sequence *before* the punch rolls the state back: a
/// lease that is refused, or that cannot be asked for. Once
/// `fallocate` has run there is nothing to roll back to — the blocks are
/// gone and the file is not `hydrated` any more — so a failure from there on
/// leaves it `dehydrating` deliberately ([`demote`]): startup recovery
/// empties whatever is left and calls it `online-only`, and the next open
/// hydrates it again.
fn punch_clean_file(file: &File, restore: FileTimes) -> Result<(), DehydrateError> {
    punch_clean_file_watched(file, restore, |_, _| {})
}

/// The two ends of the lease, as somewhere a test can stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watch {
    /// The lease has just been taken and nothing has been destroyed yet.
    UnderLease,
    /// The file is punched, its mtime is back, and it has been published
    /// `online-only`; the lease is about to be released.
    BeforeRelease,
}

/// As [`punch_clean_file`], with an observation point at each end of the
/// lease's life.
///
/// Both exist so that "nobody can open this file while it is being emptied"
/// is something a test can *measure* — an opener started at `UnderLease`
/// must still be suspended at `BeforeRelease` — rather than something a
/// reader has to infer from where a `drop` happens to sit. One point was not
/// enough: a lease released between the hook and the punch, or between the
/// punch and the state flip, both went unnoticed by a test that only asked
/// `F_GETLEASE` at the start. Production takes the empty
/// closure, which costs nothing.
fn punch_clean_file_watched(
    file: &File,
    restore: FileTimes,
    mut watch: impl FnMut(Watch, &File),
) -> Result<(), DehydrateError> {
    // Step 3. The kernel grants this only while nobody else holds the file
    // open, which is what makes emptying it safe; a refusal is "in use", and
    // the state must go back to `hydrated` before we report it. A lease that
    // cannot be asked for at all (leases switched off, a file of another
    // user, no memory for the lock) stops here just the same, with nothing
    // punched, and rolls back just the same.
    let lease = match WriteLease::take(file) {
        Ok(Some(lease)) => lease,
        Ok(None) => return Err(roll_back(file, DehydrateError::InUse)),
        Err(e) => return Err(roll_back(file, io_error(e))),
    };

    watch(Watch::UnderLease, file);

    // Steps 4–5. The lease is held across all of them: `lease` is dropped
    // below, so every open arriving from here on waits for the break instead
    // of reading a file mid-punch or a file that is empty but still says
    // `dehydrating`.
    let shape = Shape { size: None, times: restore };
    match demote(file, Keep::Nothing, shape, Held::Lease(&lease)).map_err(io_error)? {
        Demoted::Done { .. } => {}
        // Under the per-inode lock only something outside the daemon can have
        // changed the state since `mark_dehydrating`. A file that reads
        // `hydrated`, `online-only` or nothing by now is not this free-up's
        // to empty, and is refused. One that reads `hydrating` is emptied all
        // the same (`demote` under a lease takes both interrupted states):
        // its mark was cleared a moment ago and its content is not trusted.
        Demoted::Left(now) => {
            let now = now.map_or("no state", State::as_str);
            return Err(DehydrateError::Io(format!("the file's state changed to {now} while it was freed up; it was left as it is")));
        }
    }
    watch(Watch::BeforeRelease, file);
    drop(lease);
    Ok(())
}

/// The full sequence, including the helper round trip.
///
/// `root` is the registered sync root the file must live in; see
/// [`SyncRoot::open_inside`] for what that is worth and why the check is
/// here.
///
/// # Why `clear_ignore`'s error must propagate — never `let _ =`
///
/// The helper's ignore mark carries `FAN_MARK_IGNORED_SURV_MODIFY` (spec
/// §5.1, invariant M3), which removed an accidental safety net: an ignore
/// mark used to be cleared by any modification to the file, so a dehydration
/// that skipped or failed the clear used to repair itself the moment the
/// punch modified the file. It no longer does. If this were ever weakened —
/// "the clear mostly works, and worst case the helper re-derives it later" —
/// a failed `ClearIgnore` followed by a punch leaves the file **empty and
/// permanently un-intercepted**: every future open is allowed straight
/// through by the stale ignore mark and reads zeros, silently, forever
/// (until the kernel happens to evict the inode). That is the worst outcome
/// this whole sub-project exists to avoid, and it would happen with no error
/// visible anywhere past this point. Do not "simplify" this into a swallowed
/// error.
///
/// The guarantee covers every *reported* failure: `Refused`, `Timeout`,
/// `NotRunning` and `Io` all arrive here as `Err` and stop the sequence, and
/// a panic propagates out of this function, which has no `catch_unwind`. It
/// cannot cover a helper that acknowledges success without having acted —
/// that is out of the daemon's reach by construction — and it is not a
/// guarantee `punch_clean_file` makes on its own, which is why that function
/// is private.
///
/// This needs no special case for "the mark was already gone": the helper
/// acks `ClearIgnore` with errno 0 both when it actually removed the mark
/// and when the mark was never there or the kernel had already evicted it
/// (an evictable mark is designed to vanish on its own) — see
/// `konedrive-helper/src/main.rs`'s `apply`/`ClearIgnore` handling and spec
/// §5.1. So any non-zero errno reaching here means something genuinely went
/// wrong, and stopping is always the right call.
///
/// # Cancelling this leaves the file `dehydrating`
///
/// Every failure *this function reports* rolls the state back or is past the
/// point where there is anything to roll back to. Dropping the future does
/// not: between `mark_dehydrating` and the punch the file reads
/// `dehydrating` on disk, and a caller that cancels there (a shutdown, a
/// `tokio::time::timeout`, a `select!` losing a race) leaves it that way,
/// with its blocks intact and its ignore mark possibly already cleared.
/// That is a safe state, not a lost one — the helper treats `dehydrating`
/// as "hydrate it again", and [`recover`](crate::hydration::recovery::recover) punches and relabels
/// it at the next start — but it is not a *tidy* one, so a caller that can
/// cancel should prefer to let the sequence finish.
pub async fn dehydrate(
    link: &HelperLink,
    root: &SyncRoot,
    path: &Path,
) -> Result<(), DehydrateError> {
    // The file work is blocking and belongs on a blocking
    // thread.
    let target = path.to_path_buf();
    let owned_root = root.clone();
    let file = tokio::task::spawn_blocking(move || owned_root.open_inside(&target))
        .await
        .map_err(|e| DehydrateError::Io(format!("the dehydration task failed: {e}")))??;
    dehydrate_opened(&Clearance::Link(link.clone()), file).await
}

/// [`dehydrate`] from the descriptor on, so that a caller which had to open
/// the file itself — `SyncService::dehydrate`, which needs its inode to take
/// per-inode lock *before* anything is marked —
/// hands that same descriptor straight in rather than resolving the name a
/// second time. is thereby strengthened, not weakened: there is
/// now exactly one open per dehydration, and it is the one every step uses.
///
/// # What stands between the punch and a stale ignore mark
///
/// `clearance` is step 2, decided by the local rule on
/// [`Clearance`]: with a helper link, the helper clears the mark — for a
/// folder without interception too, where it used to be skipped; with no
/// helper bound to its socket, no mark of ours exists; with a helper bound
/// and no link, nothing is emptied and the call answers
/// [`DehydrateError::HelperNotConnected`]. `SyncService::dehydrate` passes
/// the link for an intercepted root and refuses `NoHelper` without one, since
/// a file freed up there with nothing intercepting would read zeros.
pub(crate) async fn dehydrate_opened(
    clearance: &Clearance,
    file: File,
) -> Result<(), DehydrateError> {
    // Each phase hands the descriptor on to the next, so all three still
    // operate on the single open of.
    let marked = tokio::task::spawn_blocking(move || {
        if nothing_to_free(&file)? {
            return Ok(None);
        }
        let restore = mark_dehydrating(&file)?;
        Ok::<_, DehydrateError>(Some((file, restore)))
    })
    .await
    .map_err(|e| DehydrateError::Io(format!("the dehydration task failed: {e}")))??;
    let Some((file, restore)) = marked else {
        return Ok(());
    };

    // Step 2, second half: local rule, here, where the file
    // is about to be emptied, and after `dehydrating` is durable — so a
    // mark an opener places from now on is placed through a `hydrate` path
    // that reads `dehydrating` and takes it off again. The descriptor
    // the helper is handed is the one that was just checked and marked, and
    // the one that is about to be punched: the mark cannot be cleared on one
    // inode and a hole punched in another.
    //
    // Nothing here rests on what can or cannot have happened to the file
    // before: not on "a folder without interception carries no mark that
    // matters", which was falsified three times, each time by
    // a race nobody had seen. A link means the helper clears the mark; no
    // helper bound to its socket means no group of ours, and so no mark,
    // exists; a helper bound and no link means nothing is emptied.
    if let Err(e) = clearance.clear(&file).await {
        return Err(tokio::task::spawn_blocking(move || roll_back(&file, DehydrateError::from(e)))
            .await
            .unwrap_or_else(|e| DehydrateError::Io(format!("the rollback task failed: {e}"))));
    }

    tokio::task::spawn_blocking(move || punch_clean_file(&file, restore))
        .await
        .map_err(|e| DehydrateError::Io(format!("the dehydration task failed: {e}")))?
}

#[cfg(test)]
pub(crate) mod tests;
