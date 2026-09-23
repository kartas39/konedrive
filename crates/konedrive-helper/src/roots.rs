//! Registered sync roots and the rules for what the helper will act on.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Default, Serialize, Deserialize)]
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

impl Roots {
    pub fn insert(&mut self, root: Root) {
        self.by_id.insert(root.root_id.clone(), root);
    }

    /// Who registered the root with this id, if anyone.
    ///
    /// `by_id` is keyed by a **client-chosen** string, which makes it the one
    /// map in the helper whose key is not owned by anybody (Ruling H32). The
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

    /// Removes and returns the entry with this id, whoever owns it.
    ///
    /// Used only by `register_root`, to lift a user's own previous
    /// registration out of the way before the nesting check runs — a root
    /// re-announcing itself must not be reported as overlapping itself now
    /// that `nesting_conflict` no longer skips by id. The caller puts it back
    /// if the registration does not go through.
    pub fn take(&mut self, root_id: &str) -> Option<Root> {
        self.by_id.remove(root_id)
    }

    /// Removes a root, but only if the asking user is the one who registered
    /// it. Returns the entry that was removed, or `None` if there was nothing
    /// to remove or it belongs to somebody else.
    ///
    /// The entry itself comes back, rather than a bare `bool`, because
    /// unregistering has to *undo* the registration: the caller needs the
    /// path, `(dev, ino)` and uid to find the tree again and take its marks
    /// off (Ruling H58), and needs the whole entry to put back if the state
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

    /// Whether this user has registered any root at all. Ruling H15 uses it:
    /// the daemon's pid is exempt from interception only for a connection that
    /// owns a root, so merely connecting to the socket buys nothing.
    pub fn has_root_for(&self, uid: u32) -> bool {
        self.by_id.values().any(|root| root.uid == uid)
    }

    /// Spec §6.2: a root may not be nested in, nor contain, another root.
    /// Overlapping roots would mean two daemons claiming the same file and two
    /// marks on the same directory, with no rule for which one an event
    /// belongs to.
    ///
    /// **Every** registered root is compared, including one that happens to
    /// carry the same id (Ruling H32). Skipping by id was how a second user
    /// reusing somebody's `root_id` slipped past this check entirely: the
    /// victim's root was not even considered as an overlap. A daemon
    /// re-announcing its own root takes its previous entry out with
    /// [`take`](Self::take) before asking, so it is still never compared
    /// against itself.
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

/// Whether `peer_uid` may have the ignore mark taken off an object (Ruling
/// H146): a regular file it owns, wherever it lives — no registered root
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
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    fn root(uid: u32, dev: u64, ino: u64) -> Root {
        Root {
            uid,
            dev,
            ino,
            path: format!("/home/u{uid}/OneDrive"),
            root_id: format!("r{ino}"),
        }
    }

    #[test]
    fn accepts_objects_on_a_registered_root_of_the_same_user() {
        let mut roots = Roots::default();
        roots.insert(root(1000, 42, 7));
        assert!(roots.may_act_on(1000, 42, 1000));
    }

    #[test]
    fn refuses_another_users_objects_and_other_filesystems() {
        let mut roots = Roots::default();
        roots.insert(root(1000, 42, 7));
        assert!(!roots.may_act_on(1001, 42, 1001), "wrong uid");
        assert!(!roots.may_act_on(1000, 43, 1000), "wrong filesystem");
        assert!(!roots.may_act_on(1000, 42, 1001), "object owned by someone else");
    }

    #[test]
    fn round_trips_through_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("roots.json");
        let mut roots = Roots::default();
        roots.insert(root(1000, 42, 7));
        roots.save(&path).unwrap();
        let loaded = Roots::load(&path).unwrap();
        assert!(loaded.may_act_on(1000, 42, 1000));
    }

    #[test]
    fn a_missing_state_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots::load(&dir.path().join("absent.json")).unwrap();
        assert!(!roots.may_act_on(1000, 42, 1000));
    }

    /// The file lists where every user keeps their files; the helper is root
    /// and nothing else needs to read it.
    #[test]
    fn the_state_file_is_not_world_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("roots.json");
        let mut roots = Roots::default();
        roots.insert(root(1000, 42, 7));
        roots.save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
    }

    /// `Restart=always` turns "refuse to start" into "never intercept
    /// anything", so a damaged file must cost the registrations and nothing
    /// more.
    #[test]
    fn a_corrupt_state_file_is_moved_aside_and_the_helper_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("roots.json");
        std::fs::write(&path, b"{ this is not json at all").unwrap();

        let roots = Roots::load(&path).expect("a corrupt file must not stop startup");
        assert_eq!(roots.iter().count(), 0);
        assert!(!path.exists(), "the damaged file must not be left in place to fail again");
        let aside = dir.path().join("roots.corrupt");
        assert!(aside.exists(), "and it must be kept for a human to look at");
        assert_eq!(std::fs::read(&aside).unwrap(), b"{ this is not json at all");
    }

    #[test]
    fn only_the_owner_can_unregister_a_root() {
        let mut roots = Roots::default();
        roots.insert(root(1000, 42, 7));
        assert!(
            roots.remove_owned(1001, "r7").is_none(),
            "another user must not be able to remove it"
        );
        assert!(roots.remove_owned(1000, "nonexistent").is_none());
        assert_eq!(roots.iter().count(), 1);
        let removed = roots.remove_owned(1000, "r7").expect("the owner can remove it");
        assert_eq!(removed.path, "/home/u1000/OneDrive", "and gets the entry back to undo it");
        assert_eq!(roots.iter().count(), 0);
    }

    #[test]
    fn a_root_may_not_nest_in_or_contain_another() {
        let mut roots = Roots::default();
        roots.insert(Root {
            uid: 1000,
            dev: 42,
            ino: 7,
            path: "/home/u/OneDrive".into(),
            root_id: "a".into(),
        });

        assert_eq!(
            roots.nesting_conflict("/home/u/OneDrive/Work", 42, 9),
            Some(Nesting::Inside("a".into()))
        );
        assert_eq!(
            roots.nesting_conflict("/home/u", 42, 9),
            Some(Nesting::Contains("a".into()))
        );
        assert_eq!(
            roots.nesting_conflict("/home/u/OneDrive", 42, 7),
            Some(Nesting::SameDirectory("a".into())),
            "the same directory under a second id is the same overlap"
        );

        // Re-announcing an existing root is still allowed, but only by
        // lifting the previous entry out first — the check itself no longer
        // trusts a matching id (Ruling H32).
        let previous = roots.take("a").expect("the entry is there");
        assert_eq!(roots.nesting_conflict("/home/u/OneDrive", 42, 7), None);
        roots.insert(previous);
        assert_eq!(roots.iter().count(), 1);
    }

    /// Ruling H32. `by_id` is keyed by a string the client chooses, so the
    /// key authorises nothing on its own; the helper must ask who owns it.
    #[test]
    fn a_root_id_belongs_to_the_user_who_registered_it() {
        let mut roots = Roots::default();
        roots.insert(root(1000, 42, 7));
        assert_eq!(roots.owner_of("r7"), Some(1000));
        assert_eq!(roots.owner_of("never-registered"), None);
    }

    /// The overlap a second user could previously hide behind a reused id:
    /// with the id no longer skipped, their nested directory is refused.
    #[test]
    fn reusing_another_users_root_id_no_longer_hides_an_overlap() {
        let mut roots = Roots::default();
        roots.insert(Root {
            uid: 1000,
            dev: 42,
            ino: 7,
            path: "/home/alice/OneDrive".into(),
            root_id: "shared-id".into(),
        });
        assert_eq!(
            roots.nesting_conflict("/home/alice/OneDrive/Sub", 42, 8),
            Some(Nesting::Inside("shared-id".into())),
            "a nested path is an overlap however the asker labels it"
        );
    }

    /// The comparison is by path component, not by string prefix: a sibling
    /// whose name merely starts with the same letters is not nested.
    #[test]
    fn a_sibling_with_a_similar_name_is_not_nested() {
        let mut roots = Roots::default();
        roots.insert(Root {
            uid: 1000,
            dev: 42,
            ino: 7,
            path: "/home/u/OneDrive".into(),
            root_id: "a".into(),
        });
        assert_eq!(roots.nesting_conflict("/home/u/OneDrive2", 42, 9), None);
        assert_eq!(roots.nesting_conflict("/home/u/One", 42, 9), None);
        assert_eq!(roots.nesting_conflict("/srv/other", 43, 9), None);
    }

    /// Ruling H146: ownership of a regular file is the whole test, with no
    /// root anywhere — and not one bit less than ownership.
    #[test]
    fn clearing_an_ignore_mark_needs_only_ownership_of_a_regular_file() {
        assert!(may_clear_ignore(1000, 1000, true), "one's own file, no root needed");
        assert!(!may_clear_ignore(1000, 1001, true), "somebody else's file");
        assert!(!may_clear_ignore(1000, 1000, false), "not a regular file");
    }

    #[test]
    fn has_root_for_is_per_user() {
        let mut roots = Roots::default();
        assert!(!roots.has_root_for(1000));
        roots.insert(root(1000, 42, 7));
        assert!(roots.has_root_for(1000));
        assert!(!roots.has_root_for(1001));
    }
}
