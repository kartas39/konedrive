//! The outbox (`docs/design/writes.md` §5): what the folder holds that OneDrive does
//! not have yet — intent and progress, never the truth. The disk is the truth
//! about local changes (WR4): losing this table costs restarted uploads and
//! forgotten deletes, never a byte.
//!
//! **One live row per item.** A detection merges into the item's row (the
//! table in §3.5) unless that row is `running`; then one follow-up row waits
//! behind it. An item is its item id; something not uploaded yet is its local
//! object, by file handle (or inode where there is no handle).
//!
//! **Order.** Rows run in `seq` order, the order of first detection, which a
//! merge keeps. Four rules hold a row back ([`TreeStore::outbox_blockers`]):
//! an earlier row of the same item; the `mkdir` of the directory it is in,
//! whose item id it needs; for a folder's `delete` or `move-out`, every row
//! of an item the base has inside the folder; and a row that frees the name
//! in OneDrive a row takes. The last three are structural, whatever the
//! rows' `seq`: a move out of a folder detected after the folder's delete
//! must still run first, or the cloud deletes it with the folder. Where rule
//! 4 closes a circle, its edges in the circle are dropped (see
//! [`TreeStore::outbox_dependencies`]).
//!
//! Also here: `local_skipped` (what is never uploaded, §3.4 rule 2) and the
//! item's local object (`items.local_handle`).

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use rusqlite::types::{Value, ValueRef};
use rusqlite::{params, Connection, OptionalExtension};

use super::{apply, upsert, ActivityRow, Change, Kind, Row, Table, TreeError, TreeStore, ACTIVITY_KEPT, MAX_CHAIN, ROW_COLUMNS};

/// The outbox worker's own transactions.
mod worker;

/// A name the outbox worker gives an item in OneDrive while the name its
/// row takes is still another item's (§4.4, F55 (7)).
pub const SWAP_PREFIX: &str = ".konedrive-swap-";

/// The write phase's tables, created with the rest of schema 3.
/// `AUTOINCREMENT`: a `seq` is never handed out twice, so a row removed at
/// commit can never be mistaken for a new one by a worker still holding it.
pub(super) const SCHEMA: &str = "
    CREATE TABLE outbox (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        kind TEXT NOT NULL,
        item_id TEXT,
        dev INTEGER, ino INTEGER,
        rel TEXT NOT NULL,
        base_etag TEXT, base_ctag TEXT, base_parent TEXT, base_name TEXT,
        target_parent TEXT, target_name TEXT,
        state TEXT NOT NULL,
        reason TEXT, attempts INTEGER NOT NULL DEFAULT 0, next_try INTEGER,
        snapshot TEXT,
        session_url TEXT, session_expires INTEGER, session_next INTEGER,
        handle BLOB,
        confirmed INTEGER NOT NULL DEFAULT 0);
    CREATE INDEX outbox_item ON outbox(item_id);
    CREATE TABLE local_skipped (rel TEXT PRIMARY KEY, reason TEXT NOT NULL, at INTEGER NOT NULL);";

/// The `meta` key counting outbox commits: `items.local_seq` of the row a
/// commit writes (the stale-delta guard, §3.7).
pub const OUTBOX_SEQ: &str = "outbox_seq";
/// The `meta` key of a pause's end, unix seconds; `0` until resumed (§9).
pub const PAUSED_UNTIL: &str = "paused_until";

const OUTBOX_COLUMNS: &str = "seq, kind, item_id, dev, ino, rel, base_etag, base_ctag, base_parent, base_name, \
     target_parent, target_name, state, reason, attempts, next_try, snapshot, session_url, session_expires, session_next, handle, confirmed";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OutboxKind {
    Create,
    Mkdir,
    Update,
    Move,
    Delete,
    MoveOut,
}

impl OutboxKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Mkdir => "mkdir",
            Self::Update => "update",
            Self::Move => "move",
            Self::Delete => "delete",
            Self::MoveOut => "move-out",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::Create, Self::Mkdir, Self::Update, Self::Move, Self::Delete, Self::MoveOut].into_iter().find(|k| k.as_str() == value)
    }

    /// Whether the row ends with the item gone from OneDrive.
    pub fn removes(self) -> bool {
        matches!(self, Self::Delete | Self::MoveOut)
    }

    /// Whether the row sends content (`mkdir`, `move` and `delete` are
    /// metadata rows, run one at a time, §3.5).
    pub fn sends_content(self) -> bool {
        matches!(self, Self::Create | Self::Update)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutboxState {
    /// Not quiet: open for writing somewhere; examined again later.
    Waiting,
    Ready,
    Running,
    /// Failed; tried again at `next_try`.
    Retry,
    /// Needs the user: a name OneDrive refuses, too large, OneDrive full.
    Blocked,
    /// Held by the mass-delete guard until confirmed.
    Held,
}

impl OutboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Retry => "retry",
            Self::Blocked => "blocked",
            Self::Held => "held",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::Waiting, Self::Ready, Self::Running, Self::Retry, Self::Blocked, Self::Held].into_iter().find(|s| s.as_str() == value)
    }
}

/// The local object a row is about. The handle, where the filesystem gives
/// one, is the identity; the inode number is kept beside it, as the schema
/// has it, and is the identity only where there is no handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inode {
    pub dev: u64,
    pub ino: u64,
    pub handle: Option<FileHandle>,
}

impl Inode {
    pub fn same_object(&self, other: &Inode) -> bool {
        match (&self.handle, &other.handle) {
            (Some(a), Some(b)) => a == b,
            _ => self.dev == other.dev && self.ino == other.ino,
        }
    }
}

/// What a change was made against: the item as the base had it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Base {
    /// `None` when the local content derives from another version than the
    /// base's (a download not yet replaced): the cTag is the guard then.
    pub etag: Option<String>,
    pub ctag: Option<String>,
    pub parent: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    pub seq: i64,
    pub kind: OutboxKind,
    /// `None` until a `create` or `mkdir` lands.
    pub item_id: Option<String>,
    pub inode: Option<Inode>,
    /// Where it was last seen, relative to the root.
    pub rel: PathBuf,
    pub base: Option<Base>,
    /// The parent's item id: `None` while the parent is a `mkdir` still to
    /// land (the row waits for it, and finds the id by `rel` then).
    pub target_parent: Option<String>,
    pub target_name: Option<String>,
    pub state: OutboxState,
    pub reason: Option<String>,
    pub attempts: u32,
    pub next_try: Option<i64>,
    /// `<size> <mtime_ns>` of the content being sent.
    pub snapshot: Option<String>,
    /// A bearer credential until it expires: never logged, never published.
    pub session_url: Option<String>,
    pub session_expires: Option<i64>,
    pub session_next: Option<u64>,
    /// A removal the user confirmed through the mass-delete guard: never
    /// counted or held again.
    pub confirmed: bool,
}

impl OutboxRow {
    /// The (parent, name) the row takes the item to, as the base's pair is compared.
    fn target(&self) -> (Option<&str>, Option<&str>) {
        (self.target_parent.as_deref(), self.target_name.as_deref())
    }
}

/// What an examination found about one item, or one local object not
/// uploaded yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub kind: OutboxKind,
    pub item_id: Option<String>,
    pub inode: Option<Inode>,
    pub rel: PathBuf,
    /// `None` for a `create` or a `mkdir`.
    pub base: Option<Base>,
    pub target_parent: Option<String>,
    pub target_name: Option<String>,
    /// For a `move`: the content was checked and is the base's. A `move` is
    /// also how an examination says "the item is here": at its base place
    /// with the same content it removes a pending update, a pending delete
    /// of it, or a move.
    pub same_content: bool,
    pub state: OutboxState,
    pub reason: Option<String>,
    pub next_try: Option<i64>,
}

impl Detection {
    fn target(&self) -> (Option<&str>, Option<&str>) {
        (self.target_parent.as_deref(), self.target_name.as_deref())
    }

    fn at_base(&self, base: Option<&Base>) -> bool {
        base.is_some_and(|b| self.target_parent.is_some() && (b.parent.as_deref(), b.name.as_deref()) == self.target())
    }
}

/// What recording a detection did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    Inserted(i64),
    Merged(i64),
    /// The detection cancelled the row: a create deleted before it was
    /// sent, a move back to where the base has it.
    Removed(i64),
    Nothing,
}

/// One step of an examination's result, applied with the others in one
/// transaction ([`TreeStore::outbox_apply`]).
// Built once per examination and consumed at once: not worth a box.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxOp {
    Record(Detection),
    /// Rows under `from` are now under `to`: a directory they are in moved.
    Rebase { from: PathBuf, to: PathBuf },
    Remove(i64),
    /// The inode the item is now (a scan's refresh).
    SetHandle { item_id: String, handle: Option<FileHandle> },
    /// Something never uploaded, listed under "Not uploaded" (§3.4 rule 2).
    Skip { rel: PathBuf, reason: String },
    Unskip(PathBuf),
    /// The mass-delete guard holds a removal already waiting (unless it
    /// runs already).
    Hold { seq: i64, reason: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboxApplied {
    /// Rows inserted or merged into, by `seq`.
    pub queued: Vec<i64>,
    /// Rows removed.
    pub removed: Vec<i64>,
}

/// A committed row's answer from OneDrive.
#[derive(Debug, Clone, Copy)]
pub enum Committed<'a> {
    /// The item as Graph answered, and the local object it now is.
    Item { row: &'a Row, handle: Option<&'a FileHandle> },
    /// Deleted in OneDrive (to its recycle bin).
    Gone { item_id: &'a str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSkipped {
    pub rel: PathBuf,
    pub reason: String,
    /// When it was first listed, unix seconds.
    pub at: i64,
}

/// A path as the store keeps it: text when it is UTF-8, its bytes otherwise
/// (Linux names need not be UTF-8; such a name is blocked, and still has to
/// be listed where it is). One path always gets the same form, so equality
/// in SQL holds.
fn path_value(path: &Path) -> Value {
    match path.to_str() {
        Some(text) => Value::Text(text.to_owned()),
        None => Value::Blob(path.as_os_str().as_bytes().to_vec()),
    }
}

fn path_from(value: ValueRef<'_>) -> PathBuf {
    match value {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => PathBuf::from(OsStr::from_bytes(bytes)),
        _ => PathBuf::new(),
    }
}

/// The sets of rows that wait on one another (strongly connected, more than
/// one row): Tarjan's algorithm, without recursion.
fn circles(graph: &HashMap<i64, Vec<i64>>) -> Vec<HashSet<i64>> {
    let mut index: HashMap<i64, usize> = HashMap::new();
    let mut low: HashMap<i64, usize> = HashMap::new();
    let mut on_stack: HashSet<i64> = HashSet::new();
    let mut stack: Vec<i64> = Vec::new();
    let mut next = 0usize;
    let mut out = Vec::new();
    let mut nodes: Vec<i64> = graph.keys().copied().collect();
    nodes.sort_unstable();
    for start in nodes {
        if index.contains_key(&start) {
            continue;
        }
        index.insert(start, next);
        low.insert(start, next);
        next += 1;
        stack.push(start);
        on_stack.insert(start);
        let mut call: Vec<(i64, usize)> = vec![(start, 0)];
        while let Some(&(v, i)) = call.last() {
            let edges = graph.get(&v).map(Vec::as_slice).unwrap_or_default();
            if i < edges.len() {
                call.last_mut().expect("just read").1 += 1;
                let w = edges[i];
                if !graph.contains_key(&w) {
                    continue;
                }
                if let Some(&w_index) = index.get(&w) {
                    if on_stack.contains(&w) {
                        let v_low = low.get_mut(&v).expect("visited");
                        *v_low = (*v_low).min(w_index);
                    }
                } else {
                    index.insert(w, next);
                    low.insert(w, next);
                    next += 1;
                    stack.push(w);
                    on_stack.insert(w);
                    call.push((w, 0));
                }
                continue;
            }
            call.pop();
            let v_low = low[&v];
            if let Some(&(parent, _)) = call.last() {
                let parent_low = low.get_mut(&parent).expect("visited");
                *parent_low = (*parent_low).min(v_low);
            }
            if v_low == index[&v] {
                let mut circle = HashSet::new();
                loop {
                    let w = stack.pop().expect("v is on the stack");
                    on_stack.remove(&w);
                    circle.insert(w);
                    if w == v {
                        break;
                    }
                }
                if circle.len() > 1 {
                    out.push(circle);
                }
            }
        }
    }
    out
}

/// One local object, as rows are grouped by it: its handle, or its inode
/// where there is none.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ObjectKey {
    Handle(Vec<u8>),
    Inode(u64, u64),
}

impl ObjectKey {
    fn of(inode: &Inode) -> Self {
        match &inode.handle {
            Some(handle) => Self::Handle(handle.encode()),
            None => Self::Inode(inode.dev, inode.ino),
        }
    }
}

/// Whether the row takes the item away from its base place.
fn moves_away(row: &OutboxRow) -> bool {
    row.base.as_ref().is_some_and(|b| (b.parent.as_deref(), b.name.as_deref()) != row.target())
}

/// The (parent, name) in OneDrive a row frees: a removal's base place, or a
/// move's.
pub fn frees(row: &OutboxRow) -> Option<(&str, &str)> {
    let frees = row.kind.removes() || (matches!(row.kind, OutboxKind::Move | OutboxKind::Update) && moves_away(row));
    let base = row.base.as_ref().filter(|_| frees)?;
    Some((base.parent.as_deref()?, base.name.as_deref()?))
}

/// The (parent, name) in OneDrive a row takes: a new folder's or file's, or
/// a move's target. A place inside a folder still to be made takes nothing
/// the cloud has yet.
pub fn takes(row: &OutboxRow) -> Option<(&str, &str)> {
    let takes = matches!(row.kind, OutboxKind::Mkdir | OutboxKind::Create)
        || (matches!(row.kind, OutboxKind::Move | OutboxKind::Update) && moves_away(row));
    if !takes {
        return None;
    }
    Some((row.target_parent.as_deref()?, row.target_name.as_deref()?))
}

/// `path` is strictly below `dir` (`""` being the root).
pub fn is_under(path: &Path, dir: &Path) -> bool {
    path != dir && path.starts_with(dir)
}

fn outbox_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutboxRow> {
    let kind: String = row.get(1)?;
    let state: String = row.get(12)?;
    let dev: Option<i64> = row.get(3)?;
    let ino: Option<i64> = row.get(4)?;
    let handle: Option<Vec<u8>> = row.get(20)?;
    let handle = handle.as_deref().and_then(FileHandle::decode);
    let inode = match (dev, ino) {
        (Some(dev), Some(ino)) => Some(Inode { dev: dev as u64, ino: ino as u64, handle }),
        _ => handle.map(|handle| Inode { dev: 0, ino: 0, handle: Some(handle) }),
    };
    let base = Base { etag: row.get(6)?, ctag: row.get(7)?, parent: row.get(8)?, name: row.get(9)? };
    let has_base = base != Base::default();
    // A value no konedrive writes fails closed: the row is blocked, never
    // run as a guess.
    let (known_kind, known_state) = (OutboxKind::parse(&kind), OutboxState::parse(&state));
    let unreadable = match (known_kind, known_state) {
        (None, _) => Some(format!("unreadable kind {kind:?}")),
        (_, None) => Some(format!("unreadable state {state:?}")),
        _ => None,
    };
    Ok(OutboxRow {
        seq: row.get(0)?,
        kind: known_kind.unwrap_or(OutboxKind::Update),
        item_id: row.get(2)?,
        inode,
        rel: path_from(row.get_ref(5)?),
        base: has_base.then_some(base),
        target_parent: row.get(10)?,
        target_name: row.get(11)?,
        state: if unreadable.is_some() { OutboxState::Blocked } else { known_state.unwrap_or(OutboxState::Blocked) },
        reason: match unreadable {
            Some(why) => Some(why),
            None => row.get(13)?,
        },
        attempts: row.get::<_, i64>(14)? as u32,
        next_try: row.get(15)?,
        snapshot: row.get(16)?,
        session_url: row.get(17)?,
        session_expires: row.get(18)?,
        session_next: row.get::<_, Option<i64>>(19)?.map(|n| n as u64),
        confirmed: row.get::<_, i64>(21)? != 0,
    })
}

fn rows_where(conn: &Connection, filter: &str, params: impl rusqlite::Params) -> Result<Vec<OutboxRow>, TreeError> {
    let mut statement = conn.prepare(&format!("SELECT {OUTBOX_COLUMNS} FROM outbox {filter} ORDER BY seq"))?;
    let rows = statement.query_map(params, outbox_row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn all_rows(conn: &Connection) -> Result<Vec<OutboxRow>, TreeError> {
    rows_where(conn, "", [])
}

/// Whether `items` (the base) has no row for `id` any more. A query that
/// fails counts as not gone: the row is kept rather than dropped on an
/// ambiguous answer.
fn item_gone(tx: &rusqlite::Transaction<'_>, id: &str) -> bool {
    tx.query_row("SELECT 1 FROM items WHERE id = ?1", [id], |_| Ok(())).optional().map(|found| found.is_none()).unwrap_or(false)
}

/// The live rows of the item or local object `d` is about, oldest first.
fn rows_for(conn: &Connection, item_id: Option<&str>, inode: Option<&Inode>) -> Result<Vec<OutboxRow>, TreeError> {
    match (item_id, inode) {
        (Some(id), _) => rows_where(conn, "WHERE item_id = ?1", [id]),
        (None, Some(inode)) => Ok(rows_where(conn, "WHERE item_id IS NULL", [])?
            .into_iter()
            .filter(|row| row.inode.as_ref().is_some_and(|i| i.same_object(inode)))
            .collect()),
        (None, None) => Ok(Vec::new()),
    }
}

fn insert(conn: &Connection, row: &OutboxRow) -> Result<i64, TreeError> {
    let base = row.base.clone().unwrap_or_default();
    let (dev, ino, handle) = match &row.inode {
        Some(inode) => (Some(inode.dev as i64), Some(inode.ino as i64), inode.handle.as_ref().map(FileHandle::encode)),
        None => (None, None, None),
    };
    conn.execute(
        "INSERT INTO outbox (kind, item_id, dev, ino, rel, base_etag, base_ctag, base_parent, base_name,
                             target_parent, target_name, state, reason, attempts, next_try, snapshot,
                             session_url, session_expires, session_next, handle, confirmed)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
        params![
            row.kind.as_str(),
            row.item_id,
            dev,
            ino,
            path_value(&row.rel),
            base.etag,
            base.ctag,
            base.parent,
            base.name,
            row.target_parent,
            row.target_name,
            row.state.as_str(),
            row.reason,
            row.attempts as i64,
            row.next_try,
            row.snapshot,
            row.session_url,
            row.session_expires,
            row.session_next.map(|n| n as i64),
            handle,
            row.confirmed as i64,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn rewrite(conn: &Connection, row: &OutboxRow) -> Result<(), TreeError> {
    let base = row.base.clone().unwrap_or_default();
    let (dev, ino, handle) = match &row.inode {
        Some(inode) => (Some(inode.dev as i64), Some(inode.ino as i64), inode.handle.as_ref().map(FileHandle::encode)),
        None => (None, None, None),
    };
    conn.execute(
        "UPDATE outbox SET kind = ?2, item_id = ?3, dev = ?4, ino = ?5, rel = ?6, base_etag = ?7, base_ctag = ?8,
                base_parent = ?9, base_name = ?10, target_parent = ?11, target_name = ?12, state = ?13, reason = ?14,
                attempts = ?15, next_try = ?16, snapshot = ?17, session_url = ?18, session_expires = ?19,
                session_next = ?20, handle = ?21, confirmed = ?22
          WHERE seq = ?1",
        params![
            row.seq,
            row.kind.as_str(),
            row.item_id,
            dev,
            ino,
            path_value(&row.rel),
            base.etag,
            base.ctag,
            base.parent,
            base.name,
            row.target_parent,
            row.target_name,
            row.state.as_str(),
            row.reason,
            row.attempts as i64,
            row.next_try,
            row.snapshot,
            row.session_url,
            row.session_expires,
            row.session_next.map(|n| n as i64),
            handle,
            row.confirmed as i64,
        ],
    )?;
    Ok(())
}

fn new_row(d: &Detection, kind: OutboxKind, base: Option<Base>) -> OutboxRow {
    let removes = kind.removes();
    OutboxRow {
        seq: 0,
        kind,
        item_id: d.item_id.clone(),
        inode: d.inode.clone(),
        rel: d.rel.clone(),
        base,
        target_parent: if removes { None } else { d.target_parent.clone() },
        // A `move-out` keeps where the object was last proved to be: what a
        // later `ESTALE` is checked against.
        target_name: if removes && kind != OutboxKind::MoveOut { None } else { d.target_name.clone() },
        state: d.state,
        reason: d.reason.clone(),
        attempts: 0,
        next_try: d.next_try,
        snapshot: None,
        session_url: None,
        session_expires: None,
        session_next: None,
        confirmed: false,
    }
}

/// The kind a row of kind `row` becomes with a detection of kind `d`, or
/// `None` when the two cancel out (§3.5's table).
fn merged_kind(row: OutboxKind, d: &Detection) -> Option<OutboxKind> {
    use OutboxKind::*;
    Some(match (row, d.kind) {
        // Never sent: nothing to take back.
        (Create | Mkdir, Delete | MoveOut) => return None,
        // The newest content, at the newest place.
        (Create, _) => Create,
        (Mkdir, _) => Mkdir,
        (Update, Move) if d.same_content => Move,
        (Update, Move | Update | Create | Mkdir) => Update,
        // The base of the update is kept.
        (Update | Move | Delete | MoveOut, Delete) => Delete,
        (Update | Move | Delete | MoveOut, MoveOut) => MoveOut,
        (Move, Update | Create) => Update,
        (Move, Move | Mkdir) => Move,
        // Save-by-rename over a deleted name, or the item back again.
        (Delete | MoveOut, Update | Create) => Update,
        (Delete | MoveOut, Move | Mkdir) => Move,
    })
}

/// The row `existing` with detection `d` merged in; `None` when it goes.
fn merge(existing: &OutboxRow, d: &Detection) -> Option<OutboxRow> {
    let kind = merged_kind(existing.kind, d)?;
    let base = existing.base.clone().or_else(|| d.base.clone());
    let mut row = new_row(d, kind, base);
    row.seq = existing.seq;
    row.item_id = existing.item_id.clone().or_else(|| d.item_id.clone());
    row.inode = d.inode.clone().or_else(|| existing.inode.clone());
    row.attempts = existing.attempts;
    // A confirmed removal stays confirmed while it is still one.
    row.confirmed = existing.confirmed && kind.removes();
    if row.confirmed && d.state == OutboxState::Held {
        row.state = existing.state;
        row.reason = existing.reason.clone();
    }
    if kind == OutboxKind::Move && d.at_base(row.base.as_ref()) {
        return None;
    }
    // On its way through a temporary name: the row keeps it while the
    // detection still sees the object where the row was taking it, so that
    // a replay looks for the item there (F55 (7) (b)).
    let swapping = existing.target_name.as_deref().is_some_and(|n| n.starts_with(SWAP_PREFIX));
    if swapping && kind == existing.kind && d.rel == existing.rel && d.target_parent.as_ref().is_none_or(|p| existing.target_parent.as_ref() == Some(p)) {
        row.target_parent = existing.target_parent.clone();
        row.target_name = existing.target_name.clone();
    }
    // A failed row keeps its backoff, and a held delete stays held, unless the
    // detection itself waits, is blocked or is held.
    match d.state {
        OutboxState::Waiting | OutboxState::Blocked | OutboxState::Held => {}
        _ if existing.state == OutboxState::Retry => {
            row.state = OutboxState::Retry;
            row.reason = existing.reason.clone();
            row.next_try = existing.next_try;
        }
        _ if existing.state == OutboxState::Held && kind.removes() => {
            row.state = OutboxState::Held;
            row.reason = existing.reason.clone();
        }
        _ => {}
    }
    // An upload session belongs to one object, one place and one kind of
    // request; the snapshot is checked against the file before a resume, so
    // new content alone does not cost the progress.
    let same_object = match (&row.inode, &existing.inode) {
        (Some(a), Some(b)) => a.same_object(b),
        (None, None) => true,
        _ => false,
    };
    if kind == existing.kind && same_object && row.target() == existing.target() {
        row.snapshot = existing.snapshot.clone();
        row.session_url = existing.session_url.clone();
        row.session_expires = existing.session_expires;
        row.session_next = existing.session_next;
    }
    Some(row)
}

/// The row a detection makes behind a running one of the same item, or
/// `None` when the running row already does what it says.
fn follow_up(running: &OutboxRow, d: &Detection) -> Option<OutboxRow> {
    use OutboxKind::*;
    let pending_create = matches!(running.kind, Create | Mkdir);
    let kind = match d.kind {
        // Content again: a running row's snapshot is compared by the
        // examination, which only says so when it moved on since.
        Create | Update => Update,
        Mkdir | Move if d.target() == running.target() => return None,
        Mkdir | Move => Move,
        Delete | MoveOut if running.kind.removes() => return None,
        other => other,
    };
    // Behind a create the item has no id or base yet: the commit fills them.
    let base = if pending_create { None } else { running.base.clone() };
    Some(new_row(d, kind, base))
}

fn record(conn: &Connection, d: &Detection) -> Result<Recorded, TreeError> {
    let live = rows_for(conn, d.item_id.as_deref(), d.inode.as_ref())?;
    let running = live.iter().find(|row| row.state == OutboxState::Running);
    let pending = live.iter().rev().find(|row| row.state != OutboxState::Running);
    if let Some(existing) = pending {
        return Ok(match merge(existing, d) {
            Some(row) => {
                rewrite(conn, &row)?;
                Recorded::Merged(row.seq)
            }
            None => {
                conn.execute("DELETE FROM outbox WHERE seq = ?1", [existing.seq])?;
                Recorded::Removed(existing.seq)
            }
        });
    }
    if let Some(running) = running {
        return Ok(match follow_up(running, d) {
            Some(row) => Recorded::Inserted(insert(conn, &row)?),
            None => Recorded::Nothing,
        });
    }
    if d.kind == OutboxKind::Move && d.at_base(d.base.as_ref()) {
        return Ok(Recorded::Nothing);
    }
    Ok(Recorded::Inserted(insert(conn, &new_row(d, d.kind, d.base.clone()))?))
}

fn rebase(conn: &Connection, from: &Path, to: &Path) -> Result<(), TreeError> {
    for row in all_rows(conn)? {
        if let Ok(rest) = row.rel.strip_prefix(from) {
            if is_under(&row.rel, from) {
                conn.execute("UPDATE outbox SET rel = ?2 WHERE seq = ?1", params![row.seq, path_value(&to.join(rest))])?;
            }
        }
    }
    Ok(())
}

fn set_local_handle(conn: &Connection, id: &str, handle: Option<&FileHandle>) -> Result<(), TreeError> {
    let stored = handle.map(FileHandle::encode);
    for table in [Table::Items, Table::Staging] {
        conn.execute(&format!("UPDATE {} SET local_handle = ?2 WHERE id = ?1", table.name()), params![id, stored])?;
    }
    Ok(())
}

impl TreeStore {
    /// Applies an examination's result in one transaction: a crash leaves
    /// all of it or none, and the next examination finds the rest on disk.
    pub fn outbox_apply(&mut self, ops: &[OutboxOp], now: i64) -> Result<OutboxApplied, TreeError> {
        let tx = self.conn.transaction()?;
        let mut out = OutboxApplied::default();
        for op in ops {
            match op {
                OutboxOp::Record(d) => match record(&tx, d)? {
                    Recorded::Inserted(seq) | Recorded::Merged(seq) => out.queued.push(seq),
                    Recorded::Removed(seq) => out.removed.push(seq),
                    Recorded::Nothing => {}
                },
                OutboxOp::Rebase { from, to } => rebase(&tx, from, to)?,
                OutboxOp::Remove(seq) => {
                    if tx.execute("DELETE FROM outbox WHERE seq = ?1", [seq])? > 0 {
                        out.removed.push(*seq);
                    }
                }
                OutboxOp::SetHandle { item_id, handle } => set_local_handle(&tx, item_id, handle.as_ref())?,
                OutboxOp::Skip { rel, reason } => {
                    tx.execute(
                        "INSERT INTO local_skipped (rel, reason, at) VALUES (?1, ?2, ?3)
                         ON CONFLICT(rel) DO UPDATE SET reason = excluded.reason",
                        params![path_value(rel), reason, now],
                    )?;
                }
                OutboxOp::Hold { seq, reason } => {
                    tx.execute(
                        "UPDATE outbox SET state = 'held', reason = ?2, next_try = NULL WHERE seq = ?1 AND state != 'running'",
                        params![seq, reason],
                    )?;
                }
                OutboxOp::Unskip(rel) => {
                    tx.execute("DELETE FROM local_skipped WHERE rel = ?1", [path_value(rel)])?;
                }
            }
        }
        out.queued.sort_unstable();
        out.queued.dedup();
        out.queued.retain(|seq| !out.removed.contains(seq));
        tx.commit()?;
        Ok(out)
    }

    /// Records one detection (see [`OutboxOp::Record`]).
    pub fn outbox_record(&mut self, d: &Detection) -> Result<Recorded, TreeError> {
        let tx = self.conn.transaction()?;
        let recorded = record(&tx, d)?;
        tx.commit()?;
        Ok(recorded)
    }

    /// Every row, in `seq` order.
    pub fn outbox_rows(&self) -> Result<Vec<OutboxRow>, TreeError> {
        all_rows(&self.conn)
    }

    pub fn outbox_row(&self, seq: i64) -> Result<Option<OutboxRow>, TreeError> {
        Ok(rows_where(&self.conn, "WHERE seq = ?1", [seq])?.into_iter().next())
    }

    /// The live rows of item `id`: at most one, and a follow-up behind a
    /// running one.
    pub fn outbox_for_item(&self, id: &str) -> Result<Vec<OutboxRow>, TreeError> {
        rows_where(&self.conn, "WHERE item_id = ?1", [id])
    }

    /// The live rows of a local object with no item id yet.
    pub fn outbox_for_inode(&self, inode: &Inode) -> Result<Vec<OutboxRow>, TreeError> {
        rows_for(&self.conn, None, Some(inode))
    }

    /// The row whose local object has `handle`: for the watcher, which maps an
    /// event's object to what it concerns.
    pub fn outbox_by_handle(&self, handle: &FileHandle) -> Result<Option<OutboxRow>, TreeError> {
        Ok(rows_where(&self.conn, "WHERE handle = ?1", [handle.encode()])?.into_iter().next())
    }

    /// Rows strictly below `rel`.
    pub fn outbox_under(&self, rel: &Path) -> Result<Vec<OutboxRow>, TreeError> {
        Ok(all_rows(&self.conn)?.into_iter().filter(|row| is_under(&row.rel, rel)).collect())
    }

    /// Every row's blockers: the live rows it waits for (the module's four
    /// rules). Computed for all rows at once.
    pub fn outbox_dependencies(&self) -> Result<HashMap<i64, Vec<i64>>, TreeError> {
        let rows = all_rows(&self.conn)?;
        let mut deps: HashMap<i64, Vec<i64>> = rows.iter().map(|row| (row.seq, Vec::new())).collect();

        // 1. An earlier row of the same item, or of the same local object
        // with no id yet (a row behind a create that has not landed).
        let mut by_item: HashMap<&str, Vec<i64>> = HashMap::new();
        let mut pending_objects: HashMap<ObjectKey, Vec<i64>> = HashMap::new();
        for row in &rows {
            if let Some(id) = &row.item_id {
                by_item.entry(id.as_str()).or_default().push(row.seq);
            }
            if let (None, Some(inode)) = (&row.item_id, &row.inode) {
                pending_objects.entry(ObjectKey::of(inode)).or_default().push(row.seq);
            }
        }
        for row in &rows {
            let mut earlier: HashSet<i64> = HashSet::new();
            if let Some(id) = &row.item_id {
                earlier.extend(by_item[id.as_str()].iter().copied().filter(|&seq| seq < row.seq));
            }
            if let Some(same) = row.inode.as_ref().and_then(|inode| pending_objects.get(&ObjectKey::of(inode))) {
                earlier.extend(same.iter().copied().filter(|&seq| seq < row.seq));
            }
            deps.get_mut(&row.seq).expect("every row has an entry").extend(earlier);
        }

        // 2. The mkdir of the directory it is in.
        let mkdirs: HashMap<&Path, i64> =
            rows.iter().filter(|row| row.kind == OutboxKind::Mkdir).map(|row| (row.rel.as_path(), row.seq)).collect();
        for row in rows.iter().filter(|row| !row.kind.removes()) {
            if let Some(&mkdir) = row.rel.parent().and_then(|parent| mkdirs.get(parent)) {
                if mkdir != row.seq {
                    deps.get_mut(&row.seq).expect("every row has an entry").push(mkdir);
                }
            }
        }

        // 3. A folder leaving OneDrive waits for every row of what the
        // base has inside it: by item id, not by path, since a new directory
        // made at the same path is not the folder's.
        for folder in rows.iter().filter(|row| row.kind.removes()) {
            let Some(id) = &folder.item_id else { continue };
            let Some(item) = self.get(Table::Items, id)? else { continue };
            if item.kind != Kind::Folder {
                continue;
            }
            let inside: HashSet<String> = self.descendants(Table::Items, id)?.into_iter().collect();
            let waits: Vec<i64> = rows
                .iter()
                .filter(|row| row.seq != folder.seq && row.item_id.as_ref().is_some_and(|i| inside.contains(i)))
                .map(|row| row.seq)
                .collect();
            deps.get_mut(&folder.seq).expect("every row has an entry").extend(waits);
        }

        // 4. A row that takes a name in OneDrive waits for a row that frees
        // it — whatever their `seq`, since a merged row keeps its old one:
        // a folder removed and made again, `mv d d.old && mkdir d`, a `mkdir`
        // from an earlier batch moved over a folder deleted since. Names
        // compare without case, as OneDrive's do. Kept apart from rules 1–3
        // until the circles are known.
        let mut freeing: HashMap<(&str, String), Vec<i64>> = HashMap::new();
        for row in &rows {
            if let Some((parent, name)) = frees(row) {
                freeing.entry((parent, name.to_lowercase())).or_default().push(row.seq);
            }
        }
        let mut by_name: Vec<(i64, i64)> = Vec::new();
        for row in &rows {
            let Some((parent, name)) = takes(row) else { continue };
            for &freer in freeing.get(&(parent, name.to_lowercase())).map(Vec::as_slice).unwrap_or_default() {
                if freer != row.seq {
                    by_name.push((row.seq, freer));
                }
            }
        }

        // Rules 1–3 alone cannot wait in a circle. Rule 1 always points to a
        // lower `seq`. Rule 2 points only to `mkdir` rows, which have no id,
        // and from a row without an id rules 1 and 2 lead only to rows
        // without one, climbing the tree of paths (a follow-up is never a
        // `mkdir`). Among rows with an id, rule 3 goes strictly down the base
        // tree and rule 1 stays on one item. So every circle has a rule-4
        // edge, and every edge inside a set of rows that wait on one another
        // lies on a circle: dropping the rule-4 edges inside those sets —
        // only those, never a rule 1–3 edge between the same rows — leaves
        // no circle. (A corrupt base with a parent loop could still circle
        // through rules 1 and 3; the base's chains are bounded elsewhere.)
        //
        // A circle is a swap, a folder replaced by its own subfolder (`mv
        // F/sub F.tmp && rm -rf F && mv F.tmp F`), a folder wrapped in a new
        // one of its name (`mkdir t && mv d t/ && mv t d`), or a folder
        // replaced offline by a new one holding one of its files (`mkdir
        // X.new; mv X/keep X.new/; …; rm -rf X; mv X.new X`). Its taking row
        // then meets the name still taken, and only the worker keeps the
        // content safe. On a 409 for a taking row it GETs the item that holds
        // the (parent, name); if that id is the `item_id` of a live row whose
        // [`frees`] is that place (any state, names without case), the name
        // is only taken for now: it neither adopts it (§4.2, §5's replay),
        // nor makes a create/create copy (§6), nor retries there. Comparing
        // ids, not names, lets a replay still adopt our own folder. It takes
        // the row to `.konedrive-swap-<id>` in the target parent instead,
        // saving that name in the row before sending (WR7), and commits the
        // temporary place to `items` with a live `move` row for the final
        // name in the same step-2 transaction. Rules 2 and 3 stay, so no
        // folder is removed before what left it. The fixture the outbox worker proves this
        // on is `sync::local::tests::w5_fixture_folder_replaced_offline_keeping_one_file`.
        let mut union = deps.clone();
        for &(taker, freer) in &by_name {
            union.get_mut(&taker).expect("every row has an entry").push(freer);
        }
        let circles = circles(&union);
        let circle_of: HashMap<i64, usize> = circles.iter().enumerate().flat_map(|(n, circle)| circle.iter().map(move |&seq| (seq, n))).collect();
        for (taker, freer) in by_name {
            let inside_one_circle = circle_of.get(&taker).is_some_and(|n| circle_of.get(&freer) == Some(n));
            if !inside_one_circle {
                deps.get_mut(&taker).expect("every row has an entry").push(freer);
            }
        }
        for list in deps.values_mut() {
            list.sort_unstable();
            list.dedup();
        }
        Ok(deps)
    }

    /// The rows `seq` waits for.
    pub fn outbox_blockers(&self, seq: i64) -> Result<Vec<i64>, TreeError> {
        Ok(self.outbox_dependencies()?.remove(&seq).unwrap_or_default())
    }

    /// The rows that can run now, in `seq` order: `ready`, or `retry` whose
    /// time has come, with nothing to wait for. Which of them run at once is
    /// the worker's (metadata rows one at a time, §3.5).
    pub fn outbox_runnable(&self, now: i64) -> Result<Vec<OutboxRow>, TreeError> {
        let deps = self.outbox_dependencies()?;
        Ok(all_rows(&self.conn)?
            .into_iter()
            .filter(|row| match row.state {
                OutboxState::Ready => true,
                OutboxState::Retry => row.next_try.is_none_or(|at| at <= now),
                _ => false,
            })
            .filter(|row| deps.get(&row.seq).is_none_or(Vec::is_empty))
            .collect())
    }

    pub fn outbox_set_state(&self, seq: i64, state: OutboxState, reason: Option<&str>, next_try: Option<i64>) -> Result<(), TreeError> {
        self.conn.execute(
            "UPDATE outbox SET state = ?2, reason = ?3, next_try = ?4 WHERE seq = ?1",
            params![seq, state.as_str(), reason, next_try],
        )?;
        Ok(())
    }

    /// Counts one more failed attempt; the count after it.
    pub fn outbox_count_attempt(&self, seq: i64) -> Result<u32, TreeError> {
        self.conn.execute("UPDATE outbox SET attempts = attempts + 1 WHERE seq = ?1", [seq])?;
        let attempts: Option<i64> = self.conn.query_row("SELECT attempts FROM outbox WHERE seq = ?1", [seq], |r| r.get(0)).optional()?;
        Ok(attempts.unwrap_or(0) as u32)
    }

    pub fn outbox_set_snapshot(&self, seq: i64, snapshot: Option<&str>) -> Result<(), TreeError> {
        self.conn.execute("UPDATE outbox SET snapshot = ?2 WHERE seq = ?1", params![seq, snapshot])?;
        Ok(())
    }

    /// An upload session's progress, persisted before the first byte and
    /// after each fragment (§4.8).
    pub fn outbox_set_session(&self, seq: i64, url: Option<&str>, expires: Option<i64>, next: Option<u64>) -> Result<(), TreeError> {
        self.conn.execute(
            "UPDATE outbox SET session_url = ?2, session_expires = ?3, session_next = ?4 WHERE seq = ?1",
            params![seq, url, expires, next.map(|n| n as i64)],
        )?;
        Ok(())
    }

    /// `ConfirmDeletes`: the held removals may go, and are marked confirmed,
    /// so that the guard neither counts nor holds them again while the
    /// worker gets through them. How many.
    pub fn outbox_release_held(&self) -> Result<usize, TreeError> {
        Ok(self.conn.execute("UPDATE outbox SET state = 'ready', reason = NULL, confirmed = 1 WHERE state = 'held'", [])?)
    }

    /// `RestoreDeletes`: the held rows are dropped, and returned so that
    /// their items can be placed again from the cloud. Their items, and what
    /// is inside them, forget their local inode in the same transaction:
    /// until the reconcile places them again, an examination cannot prove
    /// them deleted (they are unproven), so nothing deletes them in smaller
    /// batches the guard would let through.
    pub fn outbox_drop_held(&mut self) -> Result<Vec<OutboxRow>, TreeError> {
        let tx = self.conn.transaction()?;
        let held = rows_where(&tx, "WHERE state = 'held'", [])?;
        for id in held.iter().filter_map(|row| row.item_id.as_deref()) {
            // In both tables, so that a cycle between staging and swap cannot
            // give the items their local objects back (the outbox on the bus).
            for table in ["items", "staging"] {
                tx.execute(
                    &format!(
                        "WITH RECURSIVE below(id, depth) AS (
                             SELECT ?1, 0
                             UNION ALL
                             SELECT c.id, b.depth + 1 FROM items c JOIN below b ON c.parent_id = b.id WHERE b.depth < {MAX_CHAIN})
                         UPDATE {table} SET local_handle = NULL WHERE id IN (SELECT id FROM below)"
                    ),
                    [id],
                )?;
            }
        }
        tx.execute("DELETE FROM outbox WHERE state = 'held'", [])?;
        tx.commit()?;
        Ok(held)
    }

    /// A held or pending `delete` or `move-out` row whose item the delta or
    /// a Full reconcile just found gone from OneDrive — `items` (already
    /// swapped in for this cycle) holds no row for it — has nothing left to
    /// send: dropped without a request. Not a `running` row: its own commit
    /// meets the `404` itself and drops it there ([`Committed::Gone`]).
    /// Returns what was dropped, so that a dropped `move-out`'s placeholder
    /// outside the folder can be tidied the way a dropped `move-out` always
    /// is (`Tidy::dropped`, `sync::upload::move_out`), and the outbox's
    /// counts and signals can be refreshed.
    pub fn outbox_drop_removed(&mut self) -> Result<Vec<OutboxRow>, TreeError> {
        let tx = self.conn.transaction()?;
        let gone: Vec<OutboxRow> = rows_where(&tx, "WHERE kind IN ('delete', 'move-out') AND state != 'running'", [])?
            .into_iter()
            .filter(|row| row.item_id.as_deref().is_some_and(|id| item_gone(&tx, id)))
            .collect();
        for row in &gone {
            tx.execute("DELETE FROM outbox WHERE seq = ?1", [row.seq])?;
        }
        tx.commit()?;
        Ok(gone)
    }

    /// The outbox commits so far (`meta` [`OUTBOX_SEQ`]).
    pub fn outbox_seq(&self) -> Result<i64, TreeError> {
        Ok(self.meta(OUTBOX_SEQ)?.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    /// Commit step 2 (§3.5), after the attributes are on the file: in one
    /// transaction, the base takes Graph's answer and the local object, with
    /// `local_seq = ++outbox_seq`; follow-ups behind a create learn its item
    /// id and base; the row goes; the activity event is written. Returns the
    /// commit's `local_seq`.
    pub fn outbox_commit(&mut self, seq: i64, committed: Committed<'_>, activity: Option<&ActivityRow>) -> Result<i64, TreeError> {
        let tx = self.conn.transaction()?;
        let local_seq = tx
            .query_row("SELECT value FROM meta WHERE key = ?1", [OUTBOX_SEQ], |r| r.get::<_, Option<String>>(0))
            .optional()?
            .flatten()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
            + 1;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![OUTBOX_SEQ, local_seq.to_string()],
        )?;
        let Some(committed_row) = rows_where(&tx, "WHERE seq = ?1", [seq])?.into_iter().next() else {
            return Err(TreeError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("outbox row {seq} is gone; nothing to commit"),
            )));
        };
        match committed {
            Committed::Item { row, handle } => {
                upsert(&tx, Table::Items, row)?;
                tx.execute(
                    "UPDATE items SET local_handle = ?2, local_seq = ?3 WHERE id = ?1",
                    params![row.id, handle.map(FileHandle::encode), local_seq],
                )?;
                // The follow-up behind it was detected against what this
                // commit made: that is its base now — and, behind a create,
                // its item id.
                let mut followers = rows_for(&tx, Some(&row.id), None)?;
                if let Some(inode) = committed_row.inode.clone() {
                    followers.extend(rows_for(&tx, None, Some(&inode))?);
                }
                for mut follower in followers.into_iter().filter(|r| r.seq != seq) {
                    follower.item_id = Some(row.id.clone());
                    follower.base = Some(Base {
                        etag: row.etag.clone(),
                        ctag: row.ctag.clone(),
                        parent: row.parent_id.clone(),
                        name: Some(row.name.clone()),
                    });
                    rewrite(&tx, &follower)?;
                }
            }
            Committed::Gone { item_id } => {
                apply(&tx, Table::Items, &[Change::Delete(item_id.to_owned())])?;
                // A delta fetched before this delete must not bring it back.
                super::reconcile::tombstone(&tx, &[item_id], local_seq)?;
            }
        }
        tx.execute("DELETE FROM outbox WHERE seq = ?1", [seq])?;
        if let Some(event) = activity {
            tx.execute(
                "INSERT INTO activity (at, kind, path, detail) VALUES (?1, ?2, ?3, ?4)",
                params![event.at, event.kind, event.path, event.detail],
            )?;
            tx.execute(
                "DELETE FROM activity WHERE id NOT IN (SELECT id FROM activity ORDER BY id DESC LIMIT ?1)",
                [ACTIVITY_KEPT as i64],
            )?;
        }
        tx.commit()?;
        Ok(local_seq)
    }

    /// What is never uploaded, by path (`NotUploaded()` adds the blocked rows).
    pub fn local_skipped(&self) -> Result<Vec<LocalSkipped>, TreeError> {
        let mut statement = self.conn.prepare("SELECT rel, reason, at FROM local_skipped ORDER BY rel")?;
        let rows = statement
            .query_map([], |row| Ok(LocalSkipped { rel: path_from(row.get_ref(0)?), reason: row.get(1)?, at: row.get(2)? }))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Records the inode item `id` is now: the placement's, a scan's
    /// refresh. In both tables, so that a cycle between staging and swap
    /// keeps it.
    pub fn set_local_handle(&self, id: &str, handle: Option<&FileHandle>) -> Result<(), TreeError> {
        set_local_handle(&self.conn, id, handle)
    }

    /// Every item forgets its local object, in both tables: the handles were
    /// taken on a filesystem the folder is no longer on.
    pub fn forget_local_handles(&self) -> Result<(), TreeError> {
        for table in [Table::Items, Table::Staging] {
            self.conn.execute(&format!("UPDATE {} SET local_handle = NULL", table.name()), [])?;
        }
        Ok(())
    }

    pub fn local_handle(&self, id: &str) -> Result<Option<FileHandle>, TreeError> {
        let stored: Option<Option<Vec<u8>>> =
            self.conn.query_row("SELECT local_handle FROM items WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        Ok(stored.flatten().as_deref().and_then(FileHandle::decode))
    }

    /// The base item whose local object has `handle`.
    pub fn item_by_handle(&self, handle: &FileHandle) -> Result<Option<Row>, TreeError> {
        let sql = format!("SELECT {ROW_COLUMNS} FROM items WHERE local_handle = ?1");
        Ok(self.conn.query_row(&sql, [handle.encode()], super::row_from).optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Placement;

    fn base_row(id: &str, parent: &str, name: &str, kind: Kind) -> Row {
        Row {
            id: id.into(),
            parent_id: Some(parent.into()),
            name: name.into(),
            kind,
            size: 3,
            mtime: 0,
            etag: Some(format!("e-{id}")),
            ctag: Some(format!("c-{id}")),
            quickxor: None,
            mime: None,
            placement: Placement::Placed,
        }
    }

    fn store(rows: &[Row]) -> TreeStore {
        let mut store = TreeStore::in_memory().unwrap();
        let mut changes = vec![Change::Root(Row { parent_id: None, name: String::new(), ..base_row("R", "", "", Kind::Folder) })];
        changes.extend(rows.iter().cloned().map(Change::Upsert));
        store.begin_staging(false).unwrap();
        store.stage(&changes).unwrap();
        store.commit_staging("link").unwrap();
        store
    }

    fn inode(n: u64) -> Inode {
        Inode { dev: 1, ino: n, handle: Some(FileHandle { kind: 1, bytes: n.to_le_bytes().to_vec() }) }
    }

    fn base_of(row: &Row) -> Base {
        Base { etag: row.etag.clone(), ctag: row.ctag.clone(), parent: row.parent_id.clone(), name: Some(row.name.clone()) }
    }

    fn detect(kind: OutboxKind, item: Option<&Row>, object: Option<Inode>, rel: &str, parent: Option<&str>) -> Detection {
        Detection {
            kind,
            item_id: item.map(|r| r.id.clone()),
            inode: object,
            rel: rel.into(),
            base: item.map(base_of),
            target_parent: parent.map(str::to_owned),
            target_name: Path::new(rel).file_name().map(|n| n.to_string_lossy().into_owned()),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
        }
    }

    fn kinds(store: &TreeStore) -> Vec<(OutboxKind, String)> {
        store.outbox_rows().unwrap().into_iter().map(|r| (r.kind, r.rel.display().to_string())).collect()
    }

    /// §3.5's coalescing table, row by row.
    #[test]
    fn detections_coalesce_into_one_live_row_per_item() {
        use OutboxKind::*;
        let a = base_row("A", "R", "a.txt", Kind::File);
        let d = base_row("D", "R", "d", Kind::Folder);
        let mut s = store(&[a.clone(), d.clone()]);

        // create + update / move → create, newest content at the newest place.
        s.outbox_record(&detect(Create, None, Some(inode(10)), "n.txt", Some("R"))).unwrap();
        s.outbox_record(&detect(Create, None, Some(inode(10)), "d/n.txt", Some("D"))).unwrap();
        assert_eq!(kinds(&s), vec![(Create, "d/n.txt".into())]);
        // create + delete → removed.
        assert!(matches!(s.outbox_record(&detect(Delete, None, Some(inode(10)), "d/n.txt", None)).unwrap(), Recorded::Removed(_)));
        assert!(kinds(&s).is_empty());

        // update + update → update; update + move → one row, move then content.
        s.outbox_record(&detect(Update, Some(&a), Some(inode(1)), "a.txt", Some("R"))).unwrap();
        s.outbox_record(&detect(Update, Some(&a), Some(inode(1)), "a.txt", Some("R"))).unwrap();
        s.outbox_record(&detect(Move, Some(&a), Some(inode(1)), "d/a.txt", Some("D"))).unwrap();
        let rows = s.outbox_rows().unwrap();
        assert_eq!((rows.len(), rows[0].kind, rows[0].target_parent.as_deref()), (1, Update, Some("D")));
        // update + delete → delete, the base of the update kept.
        s.outbox_record(&Detection { base: None, ..detect(Delete, Some(&a), None, "d/a.txt", None) }).unwrap();
        let row = &s.outbox_rows().unwrap()[0];
        assert_eq!((row.kind, row.base.as_ref().and_then(|b| b.etag.as_deref()), row.target_name.as_deref()), (Delete, Some("e-A"), None));
        // delete + a new file at the same name → update (save-by-rename).
        s.outbox_record(&detect(Update, Some(&a), Some(inode(2)), "a.txt", Some("R"))).unwrap();
        let row = &s.outbox_rows().unwrap()[0];
        assert_eq!((row.kind, row.inode.clone()), (Update, Some(inode(2))));
        // Checked and the same again: a move back to the base place → removed.
        s.outbox_record(&Detection { same_content: true, ..detect(Move, Some(&a), Some(inode(2)), "a.txt", Some("R")) }).unwrap();
        assert!(kinds(&s).is_empty());

        // move + move → one move to the final place; back where the base has it → removed.
        s.outbox_record(&detect(Move, Some(&a), Some(inode(1)), "d/a.txt", Some("D"))).unwrap();
        s.outbox_record(&detect(Move, Some(&a), Some(inode(1)), "d/b.txt", Some("D"))).unwrap();
        assert_eq!(kinds(&s), vec![(Move, "d/b.txt".into())]);
        s.outbox_record(&detect(Move, Some(&a), Some(inode(1)), "a.txt", Some("R"))).unwrap();
        assert!(kinds(&s).is_empty());
        // A move at the base place with no row is nothing at all.
        assert_eq!(s.outbox_record(&detect(Move, Some(&a), Some(inode(1)), "a.txt", Some("R"))).unwrap(), Recorded::Nothing);

        // move + delete → delete.
        s.outbox_record(&detect(Move, Some(&a), Some(inode(1)), "d/a.txt", Some("D"))).unwrap();
        s.outbox_record(&detect(Delete, Some(&a), None, "d/a.txt", None)).unwrap();
        assert_eq!(kinds(&s), vec![(Delete, "d/a.txt".into())]);
    }

    /// A detection never merges into a running row: one follow-up waits
    /// behind it, and a create's follow-up learns the item id at commit.
    #[test]
    fn a_running_row_gets_one_follow_up() {
        use OutboxKind::*;
        let mut s = store(&[]);
        let Recorded::Inserted(first) = s.outbox_record(&detect(Create, None, Some(inode(7)), "n.txt", Some("R"))).unwrap() else { panic!() };
        s.outbox_set_state(first, OutboxState::Running, None, None).unwrap();
        // Where the running create takes it already: nothing new.
        assert_eq!(s.outbox_record(&detect(Move, None, Some(inode(7)), "n.txt", Some("R"))).unwrap(), Recorded::Nothing);
        // New content (the examination compares the snapshot): an update behind it.
        assert_eq!(s.outbox_record(&detect(Create, None, Some(inode(7)), "n.txt", Some("R"))).unwrap(), Recorded::Inserted(first + 1));
        assert_eq!(s.outbox_record(&detect(Create, None, Some(inode(7)), "n.txt", Some("R"))).unwrap(), Recorded::Merged(first + 1));
        let rows = s.outbox_rows().unwrap();
        assert_eq!(rows.iter().map(|r| (r.kind, r.state)).collect::<Vec<_>>(), vec![(Create, OutboxState::Running), (Update, OutboxState::Ready)]);
        assert_eq!(s.outbox_blockers(rows[1].seq).unwrap(), vec![first], "the follow-up waits for the running row");

        let committed = base_row("N", "R", "n.txt", Kind::File);
        let local_seq = s.outbox_commit(first, Committed::Item { row: &committed, handle: inode(7).handle.as_ref() }, None).unwrap();
        assert_eq!((local_seq, s.outbox_seq().unwrap()), (1, 1));
        let rows = s.outbox_rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].item_id.as_deref(), Some("N"), "the follow-up is now an update of the new item");
        assert_eq!(rows[0].base.as_ref().and_then(|b| b.etag.as_deref()), Some("e-N"));
        assert_eq!(s.item_by_handle(inode(7).handle.as_ref().unwrap()).unwrap().map(|r| r.id), Some("N".into()));
        assert!(s.outbox_blockers(rows[0].seq).unwrap().is_empty());

        // Behind a running update, the follow-up's base becomes what that
        // commit made: its If-Match is the new eTag.
        let n = base_row("N", "R", "n.txt", Kind::File);
        let running = rows[0].seq;
        s.outbox_set_state(running, OutboxState::Running, None, None).unwrap();
        s.outbox_record(&detect(Update, Some(&n), Some(inode(7)), "n.txt", Some("R"))).unwrap();
        let answered = Row { etag: Some("e-N2".into()), ..n.clone() };
        assert_eq!(s.outbox_commit(running, Committed::Item { row: &answered, handle: inode(7).handle.as_ref() }, None).unwrap(), 2);
        let rows = s.outbox_rows().unwrap();
        assert_eq!((rows.len(), rows[0].base.as_ref().and_then(|b| b.etag.as_deref())), (1, Some("e-N2")));
    }

    /// Rows run in detection order; a row waits for its parent's mkdir, and a
    /// folder's delete waits for every row inside it, even a later one.
    #[test]
    fn rows_wait_for_their_parents_mkdir_and_a_folder_delete_for_what_is_inside() {
        use OutboxKind::*;
        let d = base_row("D", "R", "d", Kind::Folder);
        let x = base_row("X", "D", "x.txt", Kind::File);
        let y = base_row("Y", "R", "y.txt", Kind::File);
        let mut s = store(&[d.clone(), x.clone(), y.clone()]);
        let seq = |r: Recorded| match r {
            Recorded::Inserted(seq) | Recorded::Merged(seq) => seq,
            other => panic!("{other:?}"),
        };
        let delete_d = seq(s.outbox_record(&detect(Delete, Some(&d), None, "d", None)).unwrap());
        let mkdir = seq(s.outbox_record(&detect(Mkdir, None, Some(inode(20)), "new", Some("R"))).unwrap());
        let create = seq(s.outbox_record(&detect(Create, None, Some(inode(21)), "new/f.txt", None)).unwrap());
        // X left d for the new folder before d went: detected after d's delete.
        let move_x = seq(s.outbox_record(&detect(Move, Some(&x), Some(inode(22)), "new/x.txt", None)).unwrap());
        let update_y = seq(s.outbox_record(&detect(Update, Some(&y), Some(inode(23)), "y.txt", Some("R"))).unwrap());

        assert_eq!(s.outbox_blockers(create).unwrap(), vec![mkdir]);
        assert_eq!(s.outbox_blockers(move_x).unwrap(), vec![mkdir]);
        assert_eq!(s.outbox_blockers(delete_d).unwrap(), vec![move_x], "structural: a later row inside the folder");
        let runnable: Vec<i64> = s.outbox_runnable(0).unwrap().iter().map(|r| r.seq).collect();
        assert_eq!(runnable, vec![mkdir, update_y]);

        s.outbox_set_state(update_y, OutboxState::Retry, Some("503"), Some(100)).unwrap();
        assert_eq!(s.outbox_runnable(99).unwrap().iter().map(|r| r.seq).collect::<Vec<_>>(), vec![mkdir]);
        assert!(s.outbox_runnable(100).unwrap().iter().any(|r| r.seq == update_y), "its time has come");
        // A merge keeps the backoff.
        s.outbox_record(&detect(Update, Some(&y), Some(inode(23)), "y.txt", Some("R"))).unwrap();
        assert_eq!(s.outbox_row(update_y).unwrap().unwrap().state, OutboxState::Retry);
    }

    /// A swap (`a` and `b` exchanged) is a circle of names only: its waits
    /// are dropped and both rows can run, the first through a temporary
    /// name (§4.4). A name freed by a later row is still waited for.
    #[test]
    fn a_swap_waits_on_nothing_and_a_later_freer_is_still_waited_for() {
        use OutboxKind::*;
        let a = base_row("A", "R", "a", Kind::File);
        let b = base_row("B", "R", "b", Kind::File);
        let c = base_row("C", "R", "c", Kind::File);
        let mut s = store(&[a.clone(), b.clone(), c.clone()]);
        s.outbox_record(&detect(Move, Some(&a), Some(inode(1)), "b", Some("R"))).unwrap();
        s.outbox_record(&detect(Move, Some(&b), Some(inode(2)), "a", Some("R"))).unwrap();
        assert_eq!(s.outbox_runnable(0).unwrap().len(), 2);
        let Recorded::Inserted(create) = s.outbox_record(&detect(Create, None, Some(inode(3)), "c", Some("R"))).unwrap() else { panic!() };
        let Recorded::Inserted(delete) = s.outbox_record(&detect(Delete, Some(&c), None, "c", None)).unwrap() else { panic!() };
        assert_eq!(s.outbox_blockers(create).unwrap(), vec![delete]);
    }

    /// A directory that moves takes the rows inside it along.
    #[test]
    fn rows_follow_a_directory_that_moved() {
        use OutboxKind::*;
        let mut s = store(&[]);
        s.outbox_record(&detect(Mkdir, None, Some(inode(1)), "a", Some("R"))).unwrap();
        s.outbox_record(&detect(Create, None, Some(inode(2)), "a/f", None)).unwrap();
        s.outbox_record(&detect(Create, None, Some(inode(3)), "ab/g", None)).unwrap();
        s.outbox_apply(&[OutboxOp::Rebase { from: "a".into(), to: "b/a".into() }], 0).unwrap();
        let rels: Vec<String> = s.outbox_rows().unwrap().iter().map(|r| r.rel.display().to_string()).collect();
        assert_eq!(rels, vec!["a", "b/a/f", "ab/g"], "the directory's own row is its detection's to move, and ab is not under a");
    }

    /// The mass-delete guard's rows wait until confirmed; restoring drops them.
    #[test]
    fn held_deletes_wait_for_a_decision() {
        use OutboxKind::*;
        let a = base_row("A", "R", "a", Kind::File);
        let b = base_row("B", "R", "b", Kind::File);
        let mut s = store(&[a.clone(), b.clone()]);
        for item in [&a, &b] {
            s.outbox_record(&Detection { state: OutboxState::Held, reason: Some("mass-delete".into()), ..detect(Delete, Some(item), None, &item.name, None) })
                .unwrap();
        }
        assert!(s.outbox_runnable(0).unwrap().is_empty());
        // A new detection of the same delete keeps it held.
        s.outbox_record(&detect(Delete, Some(&a), None, "a", None)).unwrap();
        assert_eq!(s.outbox_rows().unwrap()[0].state, OutboxState::Held);
        assert_eq!(s.outbox_release_held().unwrap(), 2);
        assert_eq!(s.outbox_runnable(0).unwrap().len(), 2);
        s.outbox_set_state(1, OutboxState::Held, None, None).unwrap();
        for id in ["A", "B"] {
            s.set_local_handle(id, inode(1).handle.as_ref()).unwrap();
        }
        let dropped = s.outbox_drop_held().unwrap();
        assert_eq!(dropped.iter().map(|r| r.item_id.clone().unwrap()).collect::<Vec<_>>(), vec!["A".to_owned()]);
        assert_eq!(s.outbox_rows().unwrap().len(), 1);
        // Restored items forget their inode until they are placed again: no
        // examination can prove them deleted meanwhile.
        assert_eq!(s.local_handle("A").unwrap(), None);
        assert!(s.local_handle("B").unwrap().is_some());
    }

    /// A forced switch's drop keeps a rename half-done under a temporary
    /// name — sent there, or the base has the item there — and drops the rest.
    #[test]
    fn a_forced_drop_keeps_a_rename_half_done() {
        use OutboxKind::*;
        let a = base_row("A", "R", "a.txt", Kind::File);
        let b = base_row("B", "R", "b.txt", Kind::File);
        let swapped = base_row("S", "R", &format!("{SWAP_PREFIX}1"), Kind::File);
        let mut s = store(&[a.clone(), b.clone(), swapped.clone()]);
        s.outbox_record(&detect(Move, Some(&a), Some(inode(1)), "b.txt", Some("R"))).unwrap();
        s.outbox_record(&detect(Move, Some(&swapped), Some(inode(2)), "c.txt", Some("R"))).unwrap();
        s.outbox_record(&detect(Update, Some(&b), Some(inode(3)), "b.txt", None)).unwrap();
        let sending = s.outbox_rows().unwrap().into_iter().find(|r| r.item_id.as_deref() == Some("A")).unwrap();
        s.outbox_set_target(sending.seq, Some("R"), Some(&format!("{SWAP_PREFIX}2"))).unwrap();
        let dropped = s.outbox_drop_all().unwrap();
        assert_eq!(dropped.iter().map(|r| r.item_id.clone().unwrap()).collect::<Vec<_>>(), vec!["B".to_owned()]);
        let mut kept: Vec<String> = s.outbox_rows().unwrap().into_iter().map(|r| r.item_id.unwrap()).collect();
        kept.sort();
        assert_eq!(kept, vec!["A".to_owned(), "S".to_owned()]);
    }

    /// the outbox on the bus: restoring held deletes forgets the items' local objects
    /// for good, even with a cycle between staging and swap: its swap cannot
    /// give them back.
    #[test]
    fn dropping_held_rows_survives_a_cycles_swap() {
        use OutboxKind::*;
        let d = base_row("D", "R", "d", Kind::Folder);
        let a = base_row("A", "D", "a", Kind::File);
        let mut s = store(&[d.clone(), a.clone()]);
        for id in ["D", "A"] {
            s.set_local_handle(id, inode(1).handle.as_ref()).unwrap();
        }
        s.outbox_record(&Detection { state: OutboxState::Held, ..detect(Delete, Some(&d), None, "d", None) }).unwrap();
        s.begin_staging(true).unwrap();
        assert_eq!(s.outbox_drop_held().unwrap().len(), 1);
        s.commit_staging("link-2").unwrap();
        assert_eq!((s.local_handle("D").unwrap(), s.local_handle("A").unwrap()), (None, None));
    }

    /// A kind or state no konedrive writes fails closed: blocked, never run.
    #[test]
    fn an_unreadable_row_is_blocked() {
        let mut s = store(&[]);
        let Recorded::Inserted(seq) = s.outbox_record(&detect(OutboxKind::Create, None, Some(inode(1)), "x", Some("R"))).unwrap() else { panic!() };
        s.conn.execute("UPDATE outbox SET kind = 'frobnicate' WHERE seq = ?1", [seq]).unwrap();
        let row = s.outbox_row(seq).unwrap().unwrap();
        assert_eq!(row.state, OutboxState::Blocked);
        assert!(row.reason.unwrap().contains("frobnicate"));
        assert!(s.outbox_runnable(i64::MAX).unwrap().is_empty());
    }

    /// A name Linux allows and JSON cannot carry is still listed where it is.
    #[test]
    fn a_path_that_is_not_utf8_is_kept_as_it_is() {
        let mut s = store(&[]);
        let rel = PathBuf::from(OsStr::from_bytes(b"dir/caf\xe9.txt"));
        s.outbox_apply(&[OutboxOp::Skip { rel: rel.clone(), reason: "fifo".into() }], 5).unwrap();
        s.outbox_record(&Detection { rel: rel.clone(), ..detect(OutboxKind::Create, None, Some(inode(4)), "x", None) }).unwrap();
        s.outbox_apply(&[OutboxOp::Rebase { from: "dir".into(), to: "moved".into() }], 5).unwrap();
        assert_eq!(s.outbox_rows().unwrap()[0].rel, PathBuf::from(OsStr::from_bytes(b"moved/caf\xe9.txt")));
        assert_eq!(s.local_skipped().unwrap(), vec![LocalSkipped { rel: rel.clone(), reason: "fifo".into(), at: 5 }]);
        s.outbox_apply(&[OutboxOp::Skip { rel: rel.clone(), reason: "socket".into() }], 9).unwrap();
        assert_eq!(s.local_skipped().unwrap()[0].at, 5, "listed once, when first seen");
        s.outbox_apply(&[OutboxOp::Unskip(rel)], 9).unwrap();
        assert!(s.local_skipped().unwrap().is_empty());
    }
}
