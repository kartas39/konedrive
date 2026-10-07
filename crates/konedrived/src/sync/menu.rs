//! What the context menu may offer for a selection (`Files.Menu`, `docs/design/pinning.md` §5).
//!
//! The answer restates what `Pin`, `Unpin`, `FreeUp` and `WebUrl` take and refuse, so
//! that a file manager shows only what would go through, and decides nothing itself:
//!
//! - [`SyncService::menu_part`] looks at one account's share of the selection with the
//!   checks those calls make themselves (`pins::pin_target`, `pins::kept`, the
//!   free-up's own preconditions), reaching each item with `Reach::Look` — nothing is
//!   opened, so nothing is downloaded, and nothing is changed;
//! - [`decide`] puts the accounts' parts together into the one answer.
//!
//! A menu is waited for, so the answer waits for nothing that can take long: the
//! selection is looked at in one pass, each descriptor closed before the next is opened,
//! and the tree store is asked one question for all of it, through its read-only
//! connection, which never waits for the store's writer.

use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::State;
use konedrive_tree::outbox::Inode;

use crate::folder::root::Reach;
use crate::sync::free_up::outbox_object;
use crate::sync::pins::{kept, pin_target, root_modes, Place};
use crate::sync::{RootSource, SyncService};

/// "Always keep on this device" in the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlwaysKeep {
    Hidden,
    /// Unchecked: checking it is `Pin`, which takes every path of the answer.
    Off,
    /// Checked: unchecking it is `Unpin`.
    On,
    /// Checked, and it cannot be unchecked: `Unpin` would refuse the paths.
    OnLocked,
}

impl AlwaysKeep {
    pub fn as_str(self) -> &'static str {
        match self {
            AlwaysKeep::Hidden => "hidden",
            AlwaysKeep::Off => "off",
            AlwaysKeep::On => "on",
            AlwaysKeep::OnLocked => "on-locked",
        }
    }
}

/// "Free up space" and "Open in OneDrive" in the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    Hidden,
    Enabled,
    /// Shown, and the call behind it would be refused.
    Disabled,
}

impl Offer {
    pub fn as_str(self) -> &'static str {
        match self {
            Offer::Hidden => "hidden",
            Offer::Enabled => "enabled",
            Offer::Disabled => "disabled",
        }
    }
}

/// Why "Free up space" is disabled: what `FreeUp` would refuse the paths for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeUpWhy {
    /// A folder above keeps one of them pinned: `blocked_by`.
    PinnedAbove,
    /// The folder is intercepted and its helper is not connected.
    NoHelper,
    /// A file among them has a change waiting to be uploaded.
    NotUploaded,
    /// The daemon cannot tell now whether one has: the account's sync has not started,
    /// or its store, or a mark on a file, cannot be read.
    Unknown,
}

impl FreeUpWhy {
    pub fn as_str(self) -> &'static str {
        match self {
            FreeUpWhy::PinnedAbove => "pinned-above",
            FreeUpWhy::NoHelper => "no-helper",
            FreeUpWhy::NotUploaded => "not-uploaded",
            FreeUpWhy::Unknown => "unknown",
        }
    }
}

/// The answer of `Files.Menu`, under the names of its keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    /// The selected paths `Pin` takes, in the order given: what `Pin`, `Unpin` or
    /// `FreeUp` is then called with.
    pub paths: Vec<String>,
    pub always_keep: AlwaysKeep,
    pub free_up: Offer,
    /// Why `free_up` is `Disabled`; `None` when it is not.
    pub free_up_why: Option<FreeUpWhy>,
    /// The name of the folder above that keeps an item pinned, when that is why
    /// `always_keep` is `OnLocked` or `free_up` is `Disabled`; empty otherwise.
    pub blocked_by: String,
    pub open_online: Offer,
    /// The path `WebUrl` is then called with; empty when `open_online` is `Hidden`.
    pub open_online_path: String,
}

/// One selected path `Pin` takes, as the menu's rules need it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Taken {
    pub is_dir: bool,
    /// It carries a pin of its own.
    pub own: bool,
    /// It is pinned: by itself, or by a folder above it.
    pub pinned: bool,
    /// A file that is downloaded.
    pub hydrated: bool,
    /// It carries an item id: `WebUrl` has a page to ask for.
    pub in_onedrive: bool,
}

/// One account's share of a selection.
#[derive(Debug, Default)]
pub struct Part {
    /// Each path given, in order: what `Pin` takes, and `None` for what it refuses.
    pub taken: Vec<Option<Taken>>,
    /// The folder `Unpin` and `FreeUp` would refuse the taken paths for
    /// (`pins::kept_by_folder`).
    pub kept_by: Option<PathBuf>,
    /// What `FreeUp` would refuse the taken paths for, if it would: the helper, then a
    /// folder above, as `FreeUp` asks them; then `Unknown` for a mark that cannot be
    /// read, before the store is asked, where `FreeUp` goes file by file.
    pub free_up_refused: Option<FreeUpWhy>,
}

/// What one pass over an account's share of a selection leaves, every descriptor closed.
#[derive(Default)]
struct Looked {
    taken: Vec<Option<Taken>>,
    /// Where each taken path stands among the pins.
    places: Vec<Place>,
    /// The downloaded files among them, as the outbox knows them.
    downloaded: Vec<(Option<String>, Inode)>,
    /// A file among them whose marks could not be read: whether a change of it waits
    /// cannot be told.
    unreadable: bool,
}

impl SyncService {
    /// This account's share of what `Files.Menu` answers about `paths`, all of them
    /// routed to this account's folder. Changes nothing and opens no file.
    ///
    /// One blocking task looks at every path, and one question goes to the tree store,
    /// through its read-only connection: whether a downloaded file among them has a
    /// change waiting to be uploaded. That sees what was last committed, so a change
    /// being recorded this moment is not in the answer; `FreeUp` asks the store's own
    /// thread, and refuses then.
    pub async fn menu_part(&self, paths: &[PathBuf]) -> Part {
        let nothing = || Part { taken: vec![None; paths.len()], ..Part::default() };
        let Some(reg) = self.record() else { return nothing() };
        let (root, given) = (reg.root.clone(), paths.to_vec());
        let looked = tokio::task::spawn_blocking(move || {
            let modes = root_modes(&root).ok()?;
            let mut looked = Looked::default();
            for path in &given {
                let Ok(target) = pin_target(&root, path, Reach::Look, &modes) else {
                    looked.taken.push(None);
                    continue;
                };
                // A file's state is read here a second time: `SyncRoot::item` read it to take
                // the path, and refused the path on an error.
                let state = if target.is_dir { Ok(None) } else { Reach::Look.state(&target.item) };
                let hydrated = matches!(state, Ok(Some(State::Hydrated)));
                looked.taken.push(Some(Taken {
                    is_dir: target.is_dir,
                    own: target.own,
                    pinned: target.own || target.pinned_above(),
                    hydrated,
                    in_onedrive: matches!(Reach::Look.item_id(&target.item), Ok(Some(_))),
                }));
                // Only a downloaded file can have a change to lose (`refuse_unuploaded`).
                if hydrated {
                    match outbox_object(&target.item, Reach::Look) {
                        Ok(object) => looked.downloaded.push(object),
                        Err(_) => looked.unreadable = true,
                    }
                } else if state.is_err() {
                    // Reached only when the mark became unreadable between the two reads.
                    looked.unreadable = true;
                }
                // The descriptor closes here, before the next path is looked at.
                looked.places.push(target.into_place());
            }
            Some(looked)
        })
        .await
        .ok()
        .flatten();
        let Some(Looked { taken, places, downloaded, unreadable }) = looked else { return nothing() };
        let kept_by = kept(places.iter().map(Place::standing)).map(|(_, folder)| folder.to_path_buf());
        // `FreeUp`'s checks: in its order, but for a mark that cannot be read, which comes
        // before any file's change waiting to be uploaded.
        let free_up_refused = if self.require_helper_to_free(&reg).is_err() {
            Some(FreeUpWhy::NoHelper)
        } else if kept_by.is_some() {
            Some(FreeUpWhy::PinnedAbove)
        } else if reg.source != RootSource::OneDrive {
            None
        } else if unreadable {
            Some(FreeUpWhy::Unknown)
        } else if downloaded.is_empty() {
            None
        } else {
            match self.tree_store() {
                None => Some(FreeUpWhy::Unknown),
                Some(store) => match store.read(move |s| s.outbox_holds_any(&downloaded)).await {
                    Ok(false) => None,
                    Ok(true) => Some(FreeUpWhy::NotUploaded),
                    Err(_) => Some(FreeUpWhy::Unknown),
                },
            }
        };
        Part { taken, kept_by, free_up_refused }
    }
}

/// The answer about the selection `paths`.
///
/// `parts` is each account's share: which of `paths` (by index) were routed to its
/// folder, and what it says of them. A path in no account's folder, and an account's
/// folder itself, is in no part. `account_folder` is whether the selection is one path
/// and that path is an account's folder itself, which `WebUrl` answers with the drive's
/// root and nothing else is offered for.
pub fn decide(paths: &[String], parts: &[(Vec<usize>, Part)], account_folder: bool) -> Menu {
    let mut taken: Vec<(usize, Taken)> = parts
        .iter()
        .flat_map(|(indices, part)| indices.iter().zip(&part.taken).filter_map(|(index, taken)| Some((*index, (*taken)?))))
        .collect();
    taken.sort_by_key(|(index, _)| *index);

    let mut menu = Menu {
        paths: taken.iter().map(|(index, _)| paths[*index].clone()).collect(),
        always_keep: AlwaysKeep::Hidden,
        free_up: Offer::Hidden,
        free_up_why: None,
        blocked_by: String::new(),
        open_online: Offer::Hidden,
        open_online_path: String::new(),
    };
    // "Open in OneDrive" is for one selected path: an item `Pin` takes, which has a page
    // once OneDrive has it, or an account's folder itself.
    if let [only] = paths {
        let offer = match taken.as_slice() {
            [(_, item)] if item.in_onedrive => Offer::Enabled,
            [_] => Offer::Disabled,
            _ if account_folder => Offer::Enabled,
            _ => Offer::Hidden,
        };
        if offer != Offer::Hidden {
            menu.open_online = offer;
            menu.open_online_path = only.clone();
        }
    }
    if taken.is_empty() {
        return menu;
    }

    let kept_by: Option<&Path> = parts.iter().find_map(|(_, part)| part.kept_by.as_deref());
    // Checked when everything is pinned. Unchecked, checking it is `Pin`, which leaves a
    // path a folder above pins as it is; checked, unchecking it is `Unpin`.
    menu.always_keep = match (taken.iter().all(|(_, item)| item.pinned), kept_by) {
        (false, _) => AlwaysKeep::Off,
        (true, None) => AlwaysKeep::On,
        (true, Some(_)) => AlwaysKeep::OnLocked,
    };
    // Offered for any folder, as on Windows, and for a file with something to free or a
    // pin to take off.
    if taken.iter().any(|(_, item)| item.is_dir || item.hydrated || item.own) {
        menu.free_up_why = parts.iter().find_map(|(_, part)| part.free_up_refused);
        menu.free_up = if menu.free_up_why.is_some() { Offer::Disabled } else { Offer::Enabled };
    }
    if let Some(folder) = kept_by {
        if menu.always_keep == AlwaysKeep::OnLocked || menu.free_up_why == Some(FreeUpWhy::PinnedAbove) {
            menu.blocked_by = folder.file_name().unwrap_or(folder.as_os_str()).to_string_lossy().into_owned();
        }
    }
    menu
}
