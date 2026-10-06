//! What the helper answers when an object that left the folder is asked for by its handle.

use std::fs::File;
use std::sync::Arc;

use konedrive_fs::handle::FileHandle;
use konedrive_tree::Store;

use crate::helper::linked::Helper;
use crate::helper::HelperError;
use crate::local::handles;

/// The helper's answer to `OpenByHandle` for an object that left the folder. The errnos are
/// read here and nowhere else in the move out: a row's step, what left a moved-out folder
/// since, the re-marking and the tidying after dropped rows each say what they do with an
/// answer, not with a number.
pub(super) enum Reach {
    /// The object, read-only: alive, the user's, and carrying an item id.
    Open(Arc<File>),
    /// `ESTALE` for a handle of the filesystem the folder is on now: no such object any
    /// more. One answer is not yet a proof (an inode that cannot be read says so every
    /// time): the row's step asks twice, and for what stands where the object last was.
    Gone,
    /// `ESTALE` while the handles the store recorded are another filesystem's (a new disk, a
    /// snapshot rolled back): every decode fails so, and the answer says nothing.
    Stale,
    /// `EPERM`: not handed over — it carries no item id, is another owner's, or is in a
    /// nested subvolume. Never "gone".
    Refused,
    /// `EAGAIN`: leased now.
    Busy,
    /// `EINVAL`: not a handle the helper can read.
    BadHandle,
    /// Any other refusal, by its errno.
    Errno(i32),
    /// The helper did not answer.
    NoHelper(HelperError),
}

/// Asks the helper for the object behind `handle`; `root` is the folder's own directory.
pub(super) async fn reach(helper: &dyn Helper, store: &Store, root: &File, handle: &FileHandle) -> Reach {
    match helper.open_by_handle(root, handle).await {
        Ok(object) => Reach::Open(Arc::new(File::from(object))),
        // Every decode failure is `ESTALE`: believed only for handles taken on the
        // filesystem the folder is on now.
        Err(HelperError::Refused(libc::ESTALE)) if handles_current(store, root).await => Reach::Gone,
        Err(HelperError::Refused(libc::ESTALE)) => Reach::Stale,
        Err(HelperError::Refused(libc::EPERM)) => Reach::Refused,
        Err(HelperError::Refused(libc::EAGAIN)) => Reach::Busy,
        Err(HelperError::Refused(libc::EINVAL)) => Reach::BadHandle,
        Err(HelperError::Refused(errno)) => Reach::Errno(errno),
        Err(other) => Reach::NoHelper(other),
    }
}

/// [`handles::current`], in a blocking section. When it cannot be asked the answer is no:
/// the `ESTALE` then says nothing, and nothing is taken for gone.
async fn handles_current(store: &Store, root: &File) -> bool {
    let Ok(root) = root.try_clone() else { return false };
    let store = store.clone();
    tokio::task::spawn_blocking(move || handles::current(&store, &root)).await.unwrap_or(false)
}
