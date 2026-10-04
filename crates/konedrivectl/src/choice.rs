use zbus::zvariant::OwnedObjectPath;

use crate::text::formats::shell_word;
use crate::{ACCOUNT_VARIABLE, FIRST_LABEL};

/// One account, as a command chooses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountInfo {
    pub path: OwnedObjectPath,
    pub id: String,
    pub label: String,
    /// Empty until the account has signed in once.
    pub email: String,
}

/// The account `wanted` names (design §5.1): the one whose id is exactly `wanted`, or whose
/// label or email is `wanted` whatever the case. The daemon refuses a label shaped like an id
/// or already used, but a label may be an email, which may be another account's, and a
/// hand-edited `config.toml` can still give two accounts one label, or one account another's
/// id as its label: so every account any of the three names counts,
/// and `Err` holds them all when there is not exactly one — none, or several, which a command
/// must refuse rather than guess between (`account remove` asks nothing).
pub fn resolve<'a>(accounts: &'a [AccountInfo], wanted: &str) -> Result<&'a AccountInfo, Vec<&'a AccountInfo>> {
    let wanted = wanted.trim();
    let lower = wanted.to_lowercase();
    let named: Vec<&AccountInfo> = accounts
        .iter()
        .filter(|a| {
            a.id == wanted || a.label.to_lowercase() == lower || (!a.email.is_empty() && a.email.to_lowercase() == lower)
        })
        .collect();
    match named.as_slice() {
        [one] => Ok(one),
        _ => Err(named),
    }
}

/// Where the name of the account to use came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `--account`.
    Option,
    /// [`ACCOUNT_VARIABLE`].
    Environment,
    /// An argument of the command itself (`account rename`, `account remove`).
    Argument,
}

/// Why a command that acts on one account has none to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoChoice {
    /// There is no account at all.
    NoAccountYet,
    /// The name given is no account's id, label or email.
    Unknown { wanted: String, source: Source, labels: Vec<String> },
    /// The name given is several accounts' id, label or email: each as `label (id)`.
    Ambiguous { wanted: String, candidates: Vec<String> },
    /// Several accounts, and none chosen.
    Several { labels: Vec<String> },
}

impl NoChoice {
    /// 2, as for any other mistake on the command line, when the command has to name an
    /// account; 1 when there is none it could name.
    pub fn exit_status(&self) -> u8 {
        match self {
            NoChoice::NoAccountYet => 1,
            NoChoice::Unknown { labels, .. } if labels.is_empty() => 1,
            _ => 2,
        }
    }
}

impl std::fmt::Display for NoChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoChoice::NoAccountYet => write!(
                f,
                "No account yet: `konedrivectl account add <label>` adds one, or `konedrivectl login` adds \
                 one called {FIRST_LABEL} and signs it in"
            ),
            NoChoice::Unknown { wanted, source: Source::Environment, labels } if labels.is_empty() => write!(
                f,
                "{ACCOUNT_VARIABLE} names the account {wanted:?}, and there are no accounts yet. \
                 `konedrivectl account add {}` adds it; or `unset {ACCOUNT_VARIABLE}`, and `konedrivectl \
                 login` adds one called {FIRST_LABEL} and signs it in",
                shell_word(wanted)
            ),
            NoChoice::Unknown { wanted, labels, .. } if labels.is_empty() => write!(
                f,
                "there is no account {wanted:?}: there are no accounts yet. `konedrivectl account add <label>` \
                 adds one"
            ),
            NoChoice::Unknown { wanted, source: Source::Environment, labels } => write!(
                f,
                "{ACCOUNT_VARIABLE} names no account: {wanted:?}. The accounts are {} (`konedrivectl account \
                 list` shows their ids and emails)",
                labels.join(", ")
            ),
            NoChoice::Unknown { wanted, labels, .. } => write!(
                f,
                "there is no account {wanted:?}. The accounts are {} (`konedrivectl account list` shows their \
                 ids and emails)",
                labels.join(", ")
            ),
            NoChoice::Ambiguous { wanted, candidates } => write!(
                f,
                "{wanted:?} names several accounts: {}. Nothing was done: name the one you mean by \
                 another of its names — its label, id or email, as `konedrivectl account list` shows them",
                candidates.join(", ")
            ),
            NoChoice::Several { labels } => {
                write!(f, "Several accounts: choose one with --account ({})", labels.join(", "))
            }
        }
    }
}

impl std::error::Error for NoChoice {}

/// The account a command acts on (design §5.1): the one `wanted` names, with where the name
/// came from; with no name, the only account there is.
pub fn choose<'a>(accounts: &'a [AccountInfo], wanted: Option<(&str, Source)>) -> Result<&'a AccountInfo, NoChoice> {
    let labels = || accounts.iter().map(|a| a.label.clone()).collect::<Vec<_>>();
    match wanted {
        Some((name, source)) => resolve(accounts, name).map_err(|matches| match matches.as_slice() {
            [] => NoChoice::Unknown { wanted: name.trim().to_owned(), source, labels: labels() },
            several => NoChoice::Ambiguous {
                wanted: name.trim().to_owned(),
                candidates: several.iter().map(|a| format!("{} ({})", a.label, a.id)).collect(),
            },
        }),
        None => match accounts {
            [] => Err(NoChoice::NoAccountYet),
            [one] => Ok(one),
            _ => Err(NoChoice::Several { labels: labels() }),
        },
    }
}

/// How a command this CLI suggests names the account it is about: `konedrivectl --account
/// <label>` whenever the bare command could act on another account — there are several, or
/// [`ACCOUNT_VARIABLE`] is set — and `konedrivectl` otherwise. `label` is `None` when no one
/// account is meant (a path in no folder, say): then `<account>` stands in for it.
pub fn command_prefix(label: Option<&str>, several: bool, variable_set: bool) -> String {
    if !several && !variable_set {
        return "konedrivectl".to_owned();
    }
    match label {
        Some(label) => format!("konedrivectl --account {}", shell_word(label)),
        None => "konedrivectl --account <account>".to_owned(),
    }
}

#[cfg(test)]
mod tests;
