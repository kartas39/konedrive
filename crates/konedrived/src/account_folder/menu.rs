//! What the context menu may offer for a selection (`Files.Menu`, `docs/design/pinning.md` §5).
//!
//! The answer restates what `Pin`, `Unpin`, `FreeUp` and `WebUrl` take and refuse, so
//! that a file manager shows only what would go through, and decides nothing itself:
//!
//! - [`SyncService::menu_part`] looks at one account's share of the selection with the
//!   checks those calls make themselves, asking the folder's mount about each path —
//!   nothing is opened, so nothing is downloaded, and nothing is changed;
//! - [`decide`] puts the accounts' parts together into the one answer.
//!
//! A menu is waited for, so the answer waits for nothing that can take long: the mount
//! answers from its own reading connection to the tree store, which never waits for the
//! store's writer.

use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::State;
use konedrive_tree::ItemKind;

use crate::account_folder::SyncService;
use crate::mount::{FreeUpDecision, PinHolder};

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
    /// A file among them has a change waiting to be uploaded.
    NotUploaded,
}

impl FreeUpWhy {
    pub fn as_str(self) -> &'static str {
        match self {
            FreeUpWhy::PinnedAbove => "pinned-above",
            FreeUpWhy::NotUploaded => "not-uploaded",
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
    /// The folder `Unpin` and `FreeUp` would refuse the taken paths for: one above them
    /// that pins them.
    pub kept_by: Option<PathBuf>,
    /// What `FreeUp` would refuse the taken paths for, if it would: a folder above, then
    /// a change waiting to be uploaded, as `FreeUp` asks them.
    pub free_up_refused: Option<FreeUpWhy>,
}

impl SyncService {
    /// This account's share of what `Files.Menu` answers about `paths`, all of them
    /// routed to this account's folder. Changes nothing and opens no file: every path is
    /// asked of the folder's mount, which answers from the tree store and from the names in
    /// the folder under it.
    pub async fn menu_part(&self, paths: &[PathBuf]) -> Part {
        let mut part = Part { taken: Vec::with_capacity(paths.len()), ..Part::default() };
        let Some(mount) = self.mount() else { return Part { taken: vec![None; paths.len()], ..Part::default() } };
        let files = mount.files_interface();
        let mut changed = false;
        for path in paths {
            let Ok(status) = files.find_path_status(path).await else {
                part.taken.push(None);
                continue;
            };
            if let (None, PinHolder::DirectoryAbove { path: by }) = (&part.kept_by, &status.pin_holder) {
                part.kept_by = Some(by.clone());
            }
            // What `FreeUp` would refuse it for once its own pin is off: a change of it that is not uploaded.
            changed |= matches!(files.decide_free_up(path, true).await, Ok(FreeUpDecision::LeaveModifiedHere | FreeUpDecision::LeaveOpenForWriting));
            part.taken.push(Some(Taken {
                is_dir: status.kind == ItemKind::Directory,
                own: status.pin_holder == PinHolder::Itself,
                pinned: status.is_pinned,
                hydrated: status.find_state_attribute() == Some(State::Hydrated),
                in_onedrive: true,
            }));
        }
        // `FreeUp`'s checks, in its order.
        part.free_up_refused = if part.kept_by.is_some() {
            Some(FreeUpWhy::PinnedAbove)
        } else if changed {
            Some(FreeUpWhy::NotUploaded)
        } else {
            None
        };
        part
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
