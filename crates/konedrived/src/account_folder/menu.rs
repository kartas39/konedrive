//! What the context menu may offer for a selection (`Files.Menu`, `docs/design/pinning.md` §4.2).
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

use std::path::PathBuf;

use konedrive_fs::placeholder::State;
use konedrive_tree::ItemKind;

use crate::account_folder::SyncService;
use crate::mount::FreeUpDecision;

/// "Always keep on this device" in the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AlwaysKeep {
    Hidden,
    /// Unchecked: checking it is `Pin`, which takes every path of the answer.
    Off,
    /// Checked: unchecking it is `Unpin`.
    On,
}

impl AlwaysKeep {
    pub fn as_str(self) -> &'static str {
        match self {
            AlwaysKeep::Hidden => "hidden",
            AlwaysKeep::Off => "off",
            AlwaysKeep::On => "on",
        }
    }
}

/// "Free up space" and "Open in OneDrive" in the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Offer {
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
    /// A file among them has a change waiting to be uploaded.
    NotUploaded,
}

impl FreeUpWhy {
    pub fn as_str(self) -> &'static str {
        match self {
            FreeUpWhy::NotUploaded => "not-uploaded",
        }
    }
}

/// The answer of `Files.Menu`, under the names of its keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    /// The selected paths `Pin` takes, in the order given: what `Pin`, `Unpin` or
    /// `FreeUp` is then called with.
    pub(crate) paths: Vec<String>,
    pub(crate) always_keep: AlwaysKeep,
    pub(crate) free_up: Offer,
    /// Why `free_up` is `Disabled`; `None` when it is not.
    pub(crate) free_up_why: Option<FreeUpWhy>,
    pub(crate) open_online: Offer,
    /// The path `WebUrl` is then called with; empty when `open_online` is `Hidden`.
    pub(crate) open_online_path: String,
}

/// One selected path `Pin` takes, as the menu's rules need it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Taken {
    pub(crate) is_dir: bool,
    /// A pin holds for it: its own, or that of a folder above it.
    pub(crate) pinned: bool,
    /// A file that is downloaded.
    pub(crate) hydrated: bool,
    /// It carries an item id: `WebUrl` has a page to ask for.
    pub(crate) in_onedrive: bool,
}

/// One account's share of a selection.
#[derive(Debug, Default)]
pub struct Part {
    /// Each path given, in order: what `Pin` takes, and `None` for what it refuses.
    pub taken: Vec<Option<Taken>>,
    /// What `FreeUp` would refuse the taken paths for, if it would: a change waiting to
    /// be uploaded.
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
            // What `FreeUp` would refuse it for once no pin holds for it: a change of it that is not uploaded.
            changed |= matches!(files.decide_free_up(path, true).await, Ok(FreeUpDecision::LeaveModifiedHere | FreeUpDecision::LeaveOpenForWriting));
            part.taken.push(Some(Taken {
                is_dir: status.kind == ItemKind::Directory,
                pinned: status.is_pinned,
                hydrated: status.find_state_attribute() == Some(State::Hydrated),
                in_onedrive: true,
            }));
        }
        part.free_up_refused = changed.then_some(FreeUpWhy::NotUploaded);
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

    // Checked when a pin holds for everything: unchecking it is `Unpin`, checking it `Pin`.
    menu.always_keep = if taken.iter().all(|(_, item)| item.pinned) { AlwaysKeep::On } else { AlwaysKeep::Off };
    // Offered for any folder, as on Windows, and for a file with something to free or a
    // pin to be let go by.
    if taken.iter().any(|(_, item)| item.is_dir || item.hydrated || item.pinned) {
        menu.free_up_why = parts.iter().find_map(|(_, part)| part.free_up_refused);
        menu.free_up = if menu.free_up_why.is_some() { Offer::Disabled } else { Offer::Enabled };
    }
    menu
}

#[cfg(test)]
mod tests;
