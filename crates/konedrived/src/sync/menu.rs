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

use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::State;

use crate::folder::root::Reach;
use crate::sync::pins::{kept, pin_target, root_modes, PinTarget};
use crate::sync::SyncService;

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

/// The answer of `Files.Menu`, under the names of its keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    /// The selected paths `Pin` takes, in the order given: what `Pin`, `Unpin` or
    /// `FreeUp` is then called with.
    pub paths: Vec<String>,
    pub always_keep: AlwaysKeep,
    pub free_up: Offer,
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
    /// `FreeUp` would refuse the taken paths: for `kept_by`, or because the folder's
    /// helper is not connected, or a file among them has a change waiting to be uploaded.
    pub free_up_refused: bool,
}

impl SyncService {
    /// This account's share of what `Files.Menu` answers about `paths`, all of them
    /// routed to this account's folder. Changes nothing and opens no file.
    pub async fn menu_part(&self, paths: &[PathBuf]) -> Part {
        let nothing = || Part { taken: vec![None; paths.len()], ..Part::default() };
        let Some(reg) = self.record() else { return nothing() };
        let (root, given) = (reg.root.clone(), paths.to_vec());
        let looked = tokio::task::spawn_blocking(move || {
            let modes = root_modes(&root).ok()?;
            let looked: Vec<Option<(PinTarget, Taken)>> = given
                .iter()
                .map(|path| {
                    let target = pin_target(&root, path, Reach::Look, &modes).ok()?;
                    let taken = Taken {
                        is_dir: target.is_dir,
                        own: target.own,
                        pinned: target.own || target.pinned_above(),
                        hydrated: !target.is_dir && matches!(Reach::Look.state(&target.item), Ok(Some(State::Hydrated))),
                        in_onedrive: matches!(Reach::Look.item_id(&target.item), Ok(Some(_))),
                    };
                    Some((target, taken))
                })
                .collect();
            Some(looked)
        })
        .await
        .ok()
        .flatten();
        let Some(looked) = looked else { return nothing() };
        let taken = looked.iter().map(|one| one.as_ref().map(|(_, taken)| *taken)).collect();
        let targets: Vec<PinTarget> = looked.into_iter().flatten().map(|(target, _)| target).collect();
        let kept_by = kept(&targets).map(|(_, folder)| folder.to_path_buf());
        let mut free_up_refused = kept_by.is_some() || self.require_helper_to_free(&reg).is_err();
        for target in targets.iter().filter(|target| !target.is_dir) {
            if free_up_refused {
                break;
            }
            free_up_refused = self.refuse_unuploaded(&target.item, target.reach, &target.shown.display().to_string()).await.is_err();
        }
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
        let refused = parts.iter().any(|(_, part)| part.free_up_refused);
        menu.free_up = if refused { Offer::Disabled } else { Offer::Enabled };
    }
    if let Some(folder) = kept_by {
        if menu.always_keep == AlwaysKeep::OnLocked || menu.free_up == Offer::Disabled {
            menu.blocked_by = folder.file_name().unwrap_or(folder.as_os_str()).to_string_lossy().into_owned();
        }
    }
    menu
}
