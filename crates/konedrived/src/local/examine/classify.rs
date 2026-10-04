use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::{self, State};
use crate::folder::locks::InodeKey;
use crate::local::batch::Batch;
use crate::local::entry::{proc_path, Entry, StateAttr, Type};
use crate::local::names;
use konedrive_fs::RESERVED_PREFIX;
use konedrive_tree::outbox::{Base, Detection, LocalSkip, OutboxKind, OutboxOp, OutboxState, Snapshot};
use konedrive_tree::{ActivityKind, ActivityRow, Kind, Row, Table};

use super::{daemon_owned, depth, ExamineError, Expect, lossy, Run};

impl Run<'_, '_> {
    pub(super) fn classify(&mut self, batch: &Batch) -> Result<(), ExamineError> {
        let mut by_id: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut unnamed: Vec<usize> = Vec::new();
        for i in 0..self.entries.len() {
            let e = &self.entries[i];
            // 1. The daemon's own names; a user's `.konedrive-*` is listed.
            if daemon_owned(&e.name) {
                continue;
            }
            if e.name.as_bytes().starts_with(RESERVED_PREFIX.as_bytes()) {
                let rel = e.rel.clone();
                self.skip(&rel, LocalSkip::ReservedName);
                continue;
            }
            // 2. Never a OneDrive object — unless its name is ignored anyway
            // (Emacs's `.#name` lock is a symlink).
            if let Some(reason) = e.ty.skip_reason() {
                if !self.ex.ignore.matches(&e.name) {
                    let rel = e.rel.clone();
                    self.skip(&rel, reason);
                }
                continue;
            }
            // 2b. On another device than the folder's (a nested Btrfs
            // subvolume, a mount): never uploaded, nor anything below it —
            // the helper cannot protect what is placed there (F72).
            if e.dev != self.root_dev {
                let rel = e.rel.clone();
                self.skip(&rel, LocalSkip::OtherDevice);
                continue;
            }
            match &e.id {
                Some(id) => by_id.entry(id.clone()).or_default().push(i),
                None => unnamed.push(i),
            }
        }
        // 6. Who is who, before anything is decided by place: a directory's
        // id says what its entries' parent is.
        let mut found: Vec<(String, usize)> = Vec::new();
        for (id, entries) in &by_id {
            if let Some(chosen) = self.resolve(id, entries)? {
                found.push((id.clone(), chosen));
            }
        }
        found.sort_by_key(|(_, i)| depth(&self.entries[*i].rel));
        for (id, i) in found {
            self.found(&id, i, batch)?;
        }
        // 7. What is missing from where it was.
        self.missing()?;
        // 3–5. What has no id (or no longer has one).
        unnamed.extend(std::mem::take(&mut self.fresh));
        unnamed.sort_by_key(|&i| depth(&self.entries[i].rel));
        unnamed.dedup();
        for i in unnamed {
            if !self.consumed.contains(&i) {
                self.unnamed(i)?;
            }
        }
        self.tidy_skipped(batch.full)?;
        Ok(())
    }

    /// Which of the entries carrying `id` is the item — the one rule of it
    /// (invariant I2). An object is the item only if the base places the
    /// item and the object is the one the base records. When the recorded
    /// object is not among those seen, or none is recorded (a rebuilt store,
    /// a placement that never reached its record), it is the object standing
    /// where the item is expected — at its base place, when a row says the
    /// item was removed: such an object takes the removal back. Every other
    /// object carrying the id is a copy, the user's own
    /// ([`stranger`](Self::stranger)), wherever the item's object is and
    /// whatever became of it: nothing is asked, and no row of the item is
    /// made from a copy. Returns the item's entry, if one is; an item with
    /// none is left to [`missing`](Self::missing), if its place was looked
    /// at.
    fn resolve(&mut self, id: &str, entries: &[usize]) -> Result<Option<usize>, ExamineError> {
        let Some(base) = self.base_row(id)? else {
            // Unknown to the base.
            let placing = self.store({ let id = id.to_owned(); move |s| s.get(Table::Staging, &id) })?.is_some();
            for &i in entries {
                let e = self.entries[i].clone();
                if placing {
                    // A reconcile is placing it right now; its swap follows.
                    self.recheck(&e);
                } else if self.pending_row(&e).is_some() {
                    // A create or mkdir between its two commit steps (§5): its
                    // replay adopts it.
                } else {
                    // Not surely nobody's: another account's folder may
                    // have it, and wait to download it where it went.
                    self.stranger(i, false)?;
                }
            }
            return Ok(None);
        };
        let want = if base.kind == Kind::Folder { Type::Dir } else { Type::File };
        let mut same: Vec<usize> = Vec::new();
        for &i in entries {
            if self.entries[i].ty == want {
                same.push(i);
            } else {
                // Its own id with the other kind: not the item.
                self.stranger(i, false)?;
            }
        }
        if same.is_empty() {
            return Ok(None);
        }
        // Group by object: one inode may have several names (hard links).
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for &i in &same {
            match groups.iter_mut().find(|g| self.entries[g[0]].same_object(&self.entries[i])) {
                Some(group) => group.push(i),
                None => groups.push(vec![i]),
            }
        }
        for group in &mut groups {
            group.sort_by(|&a, &b| self.entries[a].rel.cmp(&self.entries[b].rel));
        }
        let is_at = |run: &Self, g: &[usize], rel: &Option<PathBuf>| rel.as_ref().is_some_and(|r| g.iter().any(|&i| &run.entries[i].rel == r));
        if !self.located(id)?.is_some_and(|l| l.placed) {
            // The base does not place the item: nothing on disk is it. Placed
            // right here by the new tree, a reconcile is placing it now, and
            // its swap follows.
            let placing = self.store({ let id = id.to_owned(); move |s| s.locate(Table::Staging, &id) })?.filter(|l| l.placed).map(|l| l.rel);
            for group in &groups {
                let waits = is_at(self, group, &placing);
                for &i in group {
                    if waits {
                        let e = self.entries[i].clone();
                        self.recheck(&e);
                    } else {
                        self.stranger(i, false)?;
                    }
                }
            }
            return Ok(None);
        }
        let recorded = self.local_handle(id)?;
        // Where the item is expected. With a row that says it was removed
        // (its own, or of a folder above it), where the base has it: an
        // object there takes the removal back, and makes no move.
        let expected_rel = match self.expected(id)? {
            Expect::At(rel) => Some(rel),
            _ => self.located(id)?.map(|l| l.rel),
        };
        let original = recorded.as_ref().and_then(|h| groups.iter().position(|g| self.entries[g[0]].handle.as_ref() == Some(h)));

        // Rename to a backup, write new (vim's `file~`): the recorded inode
        // sits under an ignored name beside where the item is expected, and
        // another file stands there.
        if base.kind == Kind::File {
            if let (Some(o), Some(rel)) = (original, &expected_rel) {
                let group = groups[o].clone();
                let beside = group.iter().all(|&i| self.entries[i].dir_rel() == rel.parent().unwrap_or(Path::new("")))
                    && group.iter().any(|&i| self.ex.ignore.matches(&self.entries[i].name));
                // A new file, or one that copied the old one's attributes —
                // not one the worker is creating right now.
                let newcomer = self.at.get(rel).copied().filter(|&s| {
                    let e = &self.entries[s];
                    !group.contains(&s) && e.ty == Type::File && (e.id.is_none() || e.id.as_deref() == Some(id)) && !self.being_created(e)
                });
                if let (true, Some(s)) = (beside && !is_at(self, &group, &expected_rel), newcomer) {
                    self.backup(id, &base, &group, s)?;
                    return Ok(None);
                }
            }
        }

        // The recorded object; not seen, the one where the item is expected:
        // the object a placement or a replacement left before its record, an
        // editor's new inode that copied the attributes.
        let Some(pick) = original.or_else(|| groups.iter().position(|g| is_at(self, g, &expected_rel))) else {
            for &i in &same {
                self.stranger(i, false)?;
            }
            return Ok(None);
        };
        let group = groups[pick].clone();
        let chosen = group
            .iter()
            .copied()
            .find(|&i| Some(&self.entries[i].rel) == expected_rel.as_ref())
            .unwrap_or(group[0]);
        self.chosen.insert(id.to_owned(), chosen);
        self.consumed.insert(chosen);
        // Other names of the same object: a hard link OneDrive cannot hold.
        for &i in group.iter().filter(|&&i| i != chosen) {
            self.consumed.insert(i);
            let e = self.entries[i].clone();
            if !self.ex.ignore.matches(&e.name) {
                self.skip(&e.rel, LocalSkip::HardLink);
            }
        }
        // Other objects with its id: copies that kept its attributes.
        // With the recorded object seen, they are certainly not the item.
        let certain = original == Some(pick);
        for (n, other) in groups.iter().enumerate() {
            if n != pick {
                for &i in other {
                    self.stranger(i, certain)?;
                }
            }
        }
        Ok(Some(chosen))
    }

    /// An entry with an item id that is not the item's (a copy, a file from
    /// another folder or account, the wrong kind): downloaded, it is the
    /// user's own file, stripped and uploaded as new; a directory likewise,
    /// with its contents. A file that is not downloaded cannot be read here
    /// and is listed; so is a file with other links, whose other names
    /// stripping would change too. Only when it is `certain` that the entry
    /// is nobody's file — the item's recorded object was seen in this run —
    /// is a file not downloaded that holds no data removed
    /// ([`remove_empty`](Self::remove_empty)): "not seen" is not "gone", and
    /// the entry may be the item's own file, moved where this run did not
    /// look; and an id this store does not know may be another account's,
    /// whose move-out waits to download the file where it went.
    fn stranger(&mut self, i: usize, certain: bool) -> Result<(), ExamineError> {
        let e = self.entries[i].clone();
        let listed = |run: &mut Self, reason: LocalSkip| {
            if !run.ex.ignore.matches(&e.name) {
                run.skip(&e.rel, reason);
            }
            run.consumed.insert(i);
        };
        let strip = |opened: io::Result<File>| opened.and_then(|object| placeholder::strip_konedrive_xattrs(&object));
        let dir = self.ex.disk.dir(e.dir_rel());
        let stripped = match e.ty {
            Type::Dir => strip(dir.and_then(|dir| self.ex.disk.open_subdir(&dir, &e.name))),
            Type::File if e.hydrated() && e.nlink > 1 => {
                listed(self, LocalSkip::HardLink);
                return Ok(());
            }
            Type::File if e.hydrated() => strip(dir.and_then(|dir| self.ex.disk.open_file(&dir, &e.name))),
            _ => {
                if certain && self.remove_empty(&e) {
                    self.consumed.insert(i);
                } else {
                    listed(self, LocalSkip::NotDownloaded);
                }
                return Ok(());
            }
        };
        if self.entry_io(&e, stripped)?.is_none() {
            // Not stripped, because it was refused or because it went: it
            // is not uploaded as new, and what was listed inside it gets no
            // row in this run (`unnamed`). In any other run, and at the
            // worker, the id it may still carry is no folder to go into
            // (`upload::steps::shared::dir_id`).
            self.consumed.insert(i);
            self.unreadable.insert(e.rel.clone());
            return Ok(());
        }
        if e.ty == Type::Dir && !self.whole.contains(&e.rel) {
            self.out.recheck.tree(&e.rel);
        }
        self.out.stripped.push(e.rel.clone());
        self.entries[i].id = None;
        self.entries[i].state = StateAttr::Absent;
        self.fresh.push(i);
        Ok(())
    }

    /// A copy marked as not downloaded that holds no data — a regular file
    /// with one name and no data region (`SEEK_DATA` finds none): a
    /// placeholder of nothing, restored from a snapshot or copied beside its
    /// original — is removed, and `true` returned: nothing is lost with it,
    /// and listed it would stay a dead file for good. Said in the activity
    /// log. One that holds any data, has another name, is being filled, or
    /// cannot be checked is kept (`false`): no file with data is deleted on
    /// the strength of an attribute. Checked through its own descriptor,
    /// under the lock a fill holds, and the name against that descriptor
    /// right before the unlink. Only for an entry that is certainly not the
    /// item ([`stranger`](Self::stranger)).
    fn remove_empty(&mut self, e: &Entry) -> bool {
        use std::os::unix::fs::MetadataExt;
        if e.state != StateAttr::Known(State::OnlineOnly) {
            return false;
        }
        let Ok(dir) = self.ex.disk.dir(e.dir_rel()) else { return false };
        let Ok(file) = self.ex.disk.open_file(&dir, &e.name) else { return false };
        let Some(_guard) = InodeKey::of(&file).ok().and_then(|key| self.ex.locks.try_lock(key)) else { return false };
        let empty = |file: &File| {
            file.metadata().is_ok_and(|m| m.file_type().is_file() && m.nlink() == 1 && (m.dev(), m.ino()) == (e.dev, e.ino))
                && matches!(placeholder::read_state(file), Ok(Some(State::OnlineOnly)))
                // One hole from its start to its end. Never the block
                // count: the marks alone take a block on ext4. Where holes
                // are not reported the whole file reads as data: kept.
                && nix::unistd::lseek(file.as_fd(), 0, nix::unistd::Whence::SeekData) == Err(nix::errno::Errno::ENXIO)
        };
        let named = |dir: &File| std::fs::symlink_metadata(proc_path(dir).join(&e.name)).is_ok_and(|m| (m.dev(), m.ino()) == (e.dev, e.ino));
        if !empty(&file) || !named(&dir) || !empty(&file) {
            return false;
        }
        match self.ex.disk.remove(&dir, &e.name, false) {
            Ok(()) => {
                tracing::info!("{} was marked as not downloaded, is not its item's file and held no data; it is removed", e.rel.display());
                let path = self.root_path.as_deref().map_or_else(|| e.rel.clone(), |root| root.join(&e.rel));
                let event = ActivityRow {
                    at: self.ex.now,
                    kind: ActivityKind::Removed,
                    path: path.display().to_string(),
                    detail: format!("removed an empty copy of {}: it held no content", lossy(&e.name)),
                };
                if let Err(err) = self.store(move |s| s.add_activity(std::slice::from_ref(&event))) {
                    tracing::warn!("cannot record an activity event: {err}");
                }
                true
            }
            Err(err) => {
                tracing::warn!("cannot remove the empty copy {}: {err}", e.rel.display());
                false
            }
        }
    }

    /// Save-by-rename with a backup: item `id`'s inode (`group`) now sits
    /// under an ignored name, and `s` stands where the item is. `s` is the
    /// item's new content; the backup stays the user's, stripped if it was
    /// downloaded (a placeholder stays managed and fills on open).
    fn backup(&mut self, id: &str, base: &Row, group: &[usize], s: usize) -> Result<(), ExamineError> {
        for &i in group {
            self.consumed.insert(i);
            let e = self.entries[i].clone();
            if e.hydrated() {
                let stripped = self
                    .ex
                    .disk
                    .dir(e.dir_rel())
                    .and_then(|dir| self.ex.disk.open_file(&dir, &e.name))
                    .and_then(|file| placeholder::strip_konedrive_xattrs(&file));
                if self.entry_io(&e, stripped)?.is_some() {
                    self.out.stripped.push(e.rel);
                }
            }
        }
        self.consumed.insert(s);
        self.chosen.insert(id.to_owned(), s);
        if self.entries[s].id.is_some() {
            // A new file that copied the old one's attributes (vim does):
            // its stamp and cTag are the old version's, not its own.
            self.entries[s].state = StateAttr::Absent;
        }
        self.save_by_rename(id, base, s)
    }

    /// Item `id`'s content is now the file `s` at its place: an `update`
    /// from the new inode, which takes over the item at commit. A pending
    /// create of that file goes (never one being sent: callers exclude it).
    pub(super) fn save_by_rename(&mut self, id: &str, base: &Row, s: usize) -> Result<(), ExamineError> {
        let e = self.entries[s].clone();
        let Some((state, reason, next_try)) = self.probe_writer(&e)? else { return Ok(()) };
        if let Some(row) = self.pending_row(&e).filter(|row| row.state != OutboxState::Running) {
            self.ops.push(OutboxOp::Remove(row.seq));
        }
        let mut d = self.detection(OutboxKind::Update, id, base, &e, None);
        (d.state, d.reason, d.next_try) = (state, reason, next_try);
        self.detections.push(d);
        Ok(())
    }

    pub(super) fn detection(&self, kind: OutboxKind, id: &str, base: &Row, e: &Entry, local_ctag: Option<&str>) -> Detection {
        // The version the local content derives from: the file's own cTag
        // when it names another than the base's (a download not yet
        // replaced); the eTag guards only the base's own version.
        let same_version = local_ctag.is_none_or(|c| Some(c) == base.ctag.as_deref());
        Detection {
            kind,
            item_id: Some(id.to_owned()),
            inode: Some(e.inode()),
            rel: e.rel.clone(),
            base: Some(Base {
                etag: if same_version { base.etag.clone() } else { None },
                ctag: if same_version { base.ctag.clone() } else { local_ctag.map(str::to_owned) },
                parent: base.parent_id.clone(),
                name: Some(base.name.clone()),
            }),
            target_parent: self.dir_id(e.dir_rel()),
            target_name: Some(lossy(&e.name)),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
            size: (e.ty == Type::File).then_some(e.size),
        }
    }

    /// Rules 3–5: an entry without an item id.
    fn unnamed(&mut self, i: usize) -> Result<(), ExamineError> {
        let e = self.entries[i].clone();
        // Inside a directory this run did not strip of an id that is not
        // its own (refused, or gone meanwhile): it has no folder to go into.
        if e.dir_rel().ancestors().any(|dir| self.unreadable.contains(dir)) {
            return Ok(());
        }
        let pending = self.pending_row(&e).cloned();
        let target_parent = self.dir_id(e.dir_rel());
        // 3. Ignored — its name, or a directory of the user's own above it
        // (the outbox on the bus): stays local, and a create it had goes — unless the
        // worker is creating it right now: then it is an item already, which
        // syncs whatever its name, and the rename waits behind.
        if self.ex.ignore.matches(&e.name) || self.in_ignored_dir(e.dir_rel()) {
            let being_created = self.being_created(&e);
            if being_created {
                self.detections.push(Detection {
                    kind: OutboxKind::Move,
                    item_id: None,
                    inode: Some(e.inode()),
                    rel: e.rel.clone(),
                    base: None,
                    target_parent,
                    target_name: Some(lossy(&e.name)),
                    same_content: false,
                    state: OutboxState::Ready,
                    reason: None,
                    next_try: None,
                    size: None,
                });
            } else if let Some(row) = pending.filter(|r| matches!(r.kind, OutboxKind::Create | OutboxKind::Mkdir)) {
                self.ops.push(OutboxOp::Remove(row.seq));
            }
            // A directory newly ignored takes the new things waiting inside it
            // along: their folder is never made in OneDrive (the outbox on the bus).
            // One being made keeps them: it is an item already.
            if e.ty == Type::Dir && !being_created {
                let inside: Vec<i64> =
                    self.rows.under(&e.rel).into_iter().filter(|r| r.item_id.is_none() && r.state != OutboxState::Running).map(|r| r.seq).collect();
                self.ops.extend(inside.into_iter().map(OutboxOp::Remove));
            }
            return Ok(());
        }
        let is_dir = e.ty == Type::Dir;
        // A file over a name whose delete is still pending: save-by-rename
        // across batches (§3.5).
        if !is_dir && pending.is_none() {
            let delete =
                self.rows.at(&e.rel).find(|r| r.kind == OutboxKind::Delete && r.state != OutboxState::Running && r.item_id.is_some()).cloned();
            if let Some(row) = delete {
                let id = row.item_id.clone().expect("filtered above");
                if let Some(base) = self.base_row(&id)?.filter(|b| b.kind == Kind::File) {
                    self.consumed.insert(i);
                    return self.save_by_rename(&id, &base, i);
                }
            }
        }
        let mut d = Detection {
            kind: if is_dir { OutboxKind::Mkdir } else { OutboxKind::Create },
            item_id: None,
            inode: Some(e.inode()),
            rel: e.rel.clone(),
            base: None,
            target_parent,
            target_name: Some(lossy(&e.name)),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
            size: (!is_dir).then_some(e.size),
        };
        if let Some(row) = &pending {
            if is_dir && row.rel != e.rel {
                self.ops.push(OutboxOp::Rebase { from: row.rel.clone(), to: e.rel.clone() });
            }
            let now = Snapshot::content(e.size, e.mtime.0, e.mtime.1);
            if !is_dir && row.state == OutboxState::Running && row.snapshot_is(now) {
                // Being uploaded as it is now: only where it is matters.
                d.kind = OutboxKind::Move;
            }
        }
        let refused = names::refused(&e.name).or((!is_dir && e.size > names::MAX_FILE_SIZE).then_some(names::Refused::TooLarge));
        if let Some(refused) = refused {
            d.state = OutboxState::Blocked;
            d.reason = Some(refused.reason());
        } else if !is_dir {
            // One that cannot be opened is passed over: no row.
            let Some(probed) = self.probe_writer(&e)? else { return Ok(()) };
            (d.state, d.reason, d.next_try) = probed;
        }
        self.detections.push(d);
        Ok(())
    }
}
