//! Every row's blockers computed at once: what the tests hold the point queries of `pick` to.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::{all_rows, circles, frees, takes, Inode, OutboxKind};
use crate::{Kind, Table, TreeError, TreeStore};

/// One local object, as rows are grouped by it: its handle, or its inode
/// where there is none.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ObjectKey {
    Handle(Vec<u8>),
    Inode(u64, u64),
}

#[cfg(test)]
impl ObjectKey {
    fn of(inode: &Inode) -> Self {
        match &inode.handle {
            Some(handle) => Self::Handle(handle.encode()),
            None => Self::Inode(inode.dev, inode.ino),
        }
    }
}

impl TreeStore {
    /// Every row's blockers: the live rows it waits for (the module's four
    /// rules), computed for all rows at once, as the worker did before issue
    /// #38: what the tests hold the point queries of [`pick`] to.
    #[cfg(test)]
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
        // on is `local::tests::w5_fixture_folder_replaced_offline_keeping_one_file`.
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
}
