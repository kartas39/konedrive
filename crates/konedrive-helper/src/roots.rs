//! Registered sync roots and the rules for what the helper will act on.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use konedrive_proto::is_root_id;
use serde::{Deserialize, Serialize};

/// How many roots one uid may hold.
///
/// Every root is an entry in `roots.json`, compared against at each
/// registration and walked at each start of the helper, before it answers
/// its first open: without a bound, any local user could register
/// directories of its own until saving and starting took as long as it
/// liked. The daemon registers one root for each account whose folder is
/// intercepted, so this leaves room for many accounts. Beyond it a new root
/// is refused `EDQUOT`; a root the uid already holds can still be registered
/// again. Chosen, not measured.
pub const MAX_ROOTS_PER_UID: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Root {
    pub uid: u32,
    pub dev: u64,
    pub ino: u64,
    /// The absolute path, as it was at registration and verified to resolve to
    /// `(dev, ino)` then. It is only ever a *hint* for finding the directory
    /// again after a restart: the walk revalidates `(dev, ino)` against
    /// whatever the path opens before marking anything.
    pub path: String,
    pub root_id: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Roots {
    by_id: HashMap<String, Root>,
}

/// Why a directory cannot be registered as a root.
#[derive(Debug, PartialEq, Eq)]
pub enum Nesting {
    /// The new root is inside an existing one.
    Inside(String),
    /// The new root contains an existing one.
    Contains(String),
    /// The same directory is already registered under a different id.
    SameDirectory(String),
}

/// Why a registration is refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The id does not have the form of a root id (`EINVAL`).
    NotAnId,
    /// The id names another user's root (`EPERM`).
    AnotherUsers,
    /// The directory overlaps another root (`EINVAL`).
    Overlap(Nesting),
    /// The user already holds [`MAX_ROOTS_PER_UID`] roots (`EDQUOT`).
    TooMany,
}

impl Refused {
    /// The errno the daemon is answered.
    pub fn errno(&self) -> i32 {
        match self {
            Refused::NotAnId | Refused::Overlap(_) => libc::EINVAL,
            Refused::AnotherUsers => libc::EPERM,
            Refused::TooMany => libc::EDQUOT,
        }
    }
}

/// The registrations as they are once a root has been accepted.
#[derive(Debug)]
pub struct Accepted {
    /// Every registration, the new one among them. Nothing holds it yet: the
    /// caller saves it and then puts it in place of the registrations it was
    /// made from.
    pub roots: Roots,
    /// The entry the same user held under the same id, which the new one
    /// replaces. Its directory is the caller's to unmark when it is another
    /// directory than the new root's — and the caller's to compare with the
    /// new one first ([`Root::overlap_with`]): only *other* roots were
    /// compared here, since a root announcing itself again must not be
    /// refused for overlapping itself.
    pub displaced: Option<Root>,
}

/// What a line of the log says for an id a peer sent: a root id as it is,
/// anything else cut short and escaped, since it is any bytes the peer liked.
pub fn shown_id(id: &str) -> String {
    if is_root_id(id) {
        return id.to_owned();
    }
    shown(id, 40)
}

/// What a line of the log says for a path: escaped and cut. A directory's
/// name is its owner's to choose — line breaks and all, up to `PATH_MAX` —
/// and so is every path the helper prints for a root.
pub fn shown_path(path: &str) -> String {
    shown(path, 200)
}

/// `text` in quotes with everything but plain characters escaped, and only
/// its first `most` characters when it is longer.
fn shown(text: &str, most: usize) -> String {
    let head: String = text.chars().take(most).collect();
    if head.len() == text.len() {
        format!("{head:?}")
    } else {
        format!("{head:?}… ({} bytes)", text.len())
    }
}

/// True when `path` is `within` itself or lies below it. Compared component by
/// component, so `/home/u/OneDrive2` is not inside `/home/u/OneDrive`.
fn is_within(path: &str, within: &str) -> bool {
    let path = path.trim_end_matches('/');
    let within = within.trim_end_matches('/');
    if within.is_empty() {
        return path.starts_with('/');
    }
    path == within || path.strip_prefix(within).is_some_and(|rest| rest.starts_with('/'))
}

impl Root {
    /// Whether `other` is this very registration: the same user's, of the
    /// same directory, found by the same path.
    pub fn same_entry(&self, other: &Root) -> bool {
        (self.uid, self.dev, self.ino) == (other.uid, other.dev, other.ino)
            && self.path == other.path
            && self.root_id == other.root_id
    }

    /// How a directory at `path` would overlap this root's, going by the two
    /// paths: inside it, containing it, or at the same path. This is the
    /// question [`Roots::nesting_conflict`] asks of every *other* root; the
    /// helper asks it of an id's own previous directory before it lets the
    /// id move there (see `register_root`).
    pub fn overlap_with(&self, path: &str) -> Option<Nesting> {
        if is_within(path, &self.path) {
            return Some(Nesting::Inside(self.root_id.clone()));
        }
        if is_within(&self.path, path) {
            return Some(Nesting::Contains(self.root_id.clone()));
        }
        None
    }
}

impl Roots {
    pub fn insert(&mut self, root: Root) {
        self.by_id.insert(root.root_id.clone(), root);
    }

    /// Who registered the root with this id, if anyone.
    ///
    /// `by_id` is keyed by a **client-chosen** string, which makes it the one
    /// map in the helper whose key is not owned by anybody. The
    /// key alone therefore authorises nothing: `register_root` asks this
    /// first and refuses when the answer is some other uid. Without that
    /// check, any local user could connect to the 0666 socket and replace
    /// another user's registration by reusing its id — after which the
    /// victim's `MarkDir`/`ClearIgnore` start failing `EPERM`, their daemon
    /// loses its exemption, and the next helper restart never walks their
    /// tree at all, so every placeholder in it reads as zeros.
    pub fn owner_of(&self, root_id: &str) -> Option<u32> {
        self.by_id.get(root_id).map(|root| root.uid)
    }

    /// Decides one registration: whether `root` may be registered, and what
    /// the registrations are then. `self` is left as it is, so a refusal, or
    /// a save that fails afterwards, has nothing to put back.
    ///
    /// In this order:
    /// - the id must have the form of a root id. It is a string the peer
    ///   picks, stored in `roots.json` and written to the log;
    /// - the entry it names must be free or the same user's (see
    ///   [`owner_of`](Self::owner_of));
    /// - the user's own previous entry under this id is lifted out, so that
    ///   a root announcing itself again is not compared against itself, and
    ///   the directory must then overlap no other root
    ///   ([`nesting_conflict`](Self::nesting_conflict));
    /// - a root that replaces none of the user's must leave the user within
    ///   [`MAX_ROOTS_PER_UID`].
    pub fn with(&self, root: Root) -> Result<Accepted, Refused> {
        if !is_root_id(&root.root_id) {
            return Err(Refused::NotAnId);
        }
        if self.owner_of(&root.root_id).is_some_and(|other| other != root.uid) {
            return Err(Refused::AnotherUsers);
        }
        let mut roots = self.clone();
        let displaced = roots.by_id.remove(&root.root_id);
        if let Some(conflict) = roots.nesting_conflict(&root.path, root.dev, root.ino) {
            return Err(Refused::Overlap(conflict));
        }
        if displaced.is_none() && roots.held_by(root.uid) >= MAX_ROOTS_PER_UID {
            return Err(Refused::TooMany);
        }
        roots.insert(root);
        Ok(Accepted { roots, displaced })
    }

    /// The entry registered under this id, whoever holds it.
    pub fn get(&self, root_id: &str) -> Option<&Root> {
        self.by_id.get(root_id)
    }

    /// How many roots this user holds.
    pub fn held_by(&self, uid: u32) -> usize {
        self.by_id.values().filter(|root| root.uid == uid).count()
    }

    /// The registrations without the root `uid` holds under this id, and that
    /// root; `None` if there is no such root or it is somebody else's. As
    /// with [`with`](Self::with), `self` is left as it is. The id may have
    /// any form: a root registered before the form was checked is removed
    /// like any other.
    pub fn without(&self, uid: u32, root_id: &str) -> Option<(Roots, Root)> {
        // Asked before anything is copied: a refusal costs a lookup.
        self.by_id.get(root_id).filter(|root| root.uid == uid)?;
        let mut roots = self.clone();
        let root = roots.remove_owned(uid, root_id)?;
        Some((roots, root))
    }

    /// Removes a root, but only if the asking user is the one who registered
    /// it. Returns the entry that was removed, or `None` if there was nothing
    /// to remove or it belongs to somebody else.
    ///
    /// The entry itself comes back, rather than a bare `bool`, because
    /// unregistering has to *undo* the registration: the caller needs the
    /// path, `(dev, ino)` and uid to find the tree again and take its marks
    /// off, and needs the whole entry to put back if the state
    /// file cannot be written.
    ///
    /// Roots are filtered by uid rather than by connection: they outlive both
    /// the connection that created them and the helper itself (they are
    /// reloaded from `roots.json` at startup), so a connection id would mean
    /// a user could never unregister a root after a daemon restart.
    pub fn remove_owned(&mut self, uid: u32, root_id: &str) -> Option<Root> {
        match self.by_id.get(root_id) {
            Some(root) if root.uid == uid => self.by_id.remove(root_id),
            _ => None,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Root> {
        self.by_id.values()
    }

    /// The helper acts on an object only when the asking user owns a root on
    /// the object's filesystem AND owns the object itself. Without the second
    /// check a user could have us intercept files they can read but do not own.
    pub fn may_act_on(&self, peer_uid: u32, object_dev: u64, object_uid: u32) -> bool {
        peer_uid == object_uid && self.has_root_for(peer_uid)
            && self.by_id.values().any(|root| root.uid == peer_uid && root.dev == object_dev)
    }

    /// Whether this user has registered any root at all. uses it:
    /// the daemon's pid is exempt from interception only for a connection that
    /// owns a root, so merely connecting to the socket buys nothing.
    pub fn has_root_for(&self, uid: u32) -> bool {
        self.by_id.values().any(|root| root.uid == uid)
    }

    /// A root may not be nested in, nor contain, another root.
    /// Overlapping roots would mean two daemons claiming the same file and two
    /// marks on the same directory, with no rule for which one an event
    /// belongs to.
    ///
    /// **Every** registered root is compared, including one that happens to
    /// carry the same id. Skipping by id was how a second user
    /// reusing somebody's `root_id` slipped past this check entirely: the
    /// victim's root was not even considered as an overlap. A daemon
    /// re-announcing its own root has its previous entry taken out before
    /// the question is asked, so it is still never compared
    /// against itself ([`with`](Self::with) lifts it out).
    pub fn nesting_conflict(&self, path: &str, dev: u64, ino: u64) -> Option<Nesting> {
        self.by_id.values().find_map(|root| {
            if root.dev == dev && root.ino == ino {
                return Some(Nesting::SameDirectory(root.root_id.clone()));
            }
            if is_within(path, &root.path) {
                return Some(Nesting::Inside(root.root_id.clone()));
            }
            if is_within(&root.path, path) {
                return Some(Nesting::Contains(root.root_id.clone()));
            }
            None
        })
    }

    /// Loads the registered roots, and never lets a damaged file stop the
    /// helper starting.
    ///
    /// The unit is `Restart=always`, so refusing to start on a corrupt
    /// `roots.json` is not a safe failure: it is a crash loop, and a crash
    /// loop means no interception at all, which means every placeholder in
    /// every sync folder reads as zeros. Losing the registrations is
    /// recoverable — the daemon re-registers its root on its next connection
    /// — so the damaged file is moved aside for a human to look at and the
    /// helper comes up empty.
    pub fn load(path: &Path) -> io::Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        match serde_json::from_slice(&bytes) {
            Ok(roots) => Ok(roots),
            Err(e) => {
                let aside = path.with_extension("corrupt");
                tracing::error!(
                    "{} is not readable as JSON ({e}); moving it to {} and starting with no \
                     registered roots — each daemon will re-register on its next connection",
                    path.display(),
                    aside.display()
                );
                if let Err(e) = std::fs::rename(path, &aside) {
                    tracing::error!("could not move {} aside: {e}", path.display());
                }
                Ok(Self::default())
            }
        }
    }

    /// Writes the registrations so that a crash can leave either the old file
    /// or the new one, never a truncated one: a fresh 0600 temp file,
    /// `fsync`ed, renamed over the target, and then the directory `fsync`ed so
    /// the rename itself is durable.
    ///
    /// 0600 because the file names every user's sync folder, and the helper
    /// runs as root: there is no reason for it to be world-readable.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let parent = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let tmp = path.with_extension("tmp");
        let encoded = serde_json::to_vec_pretty(self)?;
        {
            let mut file = File::options()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&tmp)?;
            // `mode` only applies when the file is created, and a leftover
            // temp file from a previous run may be anything.
            file.set_permissions(PermissionsExt::from_mode(0o600))?;
            file.write_all(&encoded)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

/// Whether `peer_uid` may have the ignore mark taken off an object: a
/// regular file it owns, wherever it lives — no registered root
/// required, which is what a daemon whose folder is registered without
/// interception lacks.
///
/// Every other request is scoped to a device the peer holds a root on
/// ([`Roots::may_act_on`]), because adding a mark stalls whoever opens the
/// object. Removing an ignore mark is the one request that cannot hurt
/// anybody: the worst it does is send the file's next open to the helper to
/// be decided again from its state — one extra interception, never zeros —
/// and it is only ever for a file the asker could open and write anyway.
/// A directory is refused: nothing of ours puts an ignore mark on one.
pub fn may_clear_ignore(peer_uid: u32, object_uid: u32, is_regular_file: bool) -> bool {
    is_regular_file && peer_uid == object_uid
}

#[cfg(test)]
mod tests;
