use std::path::Path;

use anyhow::{bail, Context};
use konedrive_dbus::accounts::{AccountProxy, AccountsProxy, FolderProxies};
use konedrive_dbus::Refusal;
use konedrivectl::choice::{self, AccountInfo, Source};
use konedrivectl::ACCOUNT_VARIABLE;
use zbus::zvariant::OwnedObjectPath;

/// The daemon: the accounts manager, and each account's objects.
pub(crate) struct Daemon {
    pub(crate) connection: zbus::Connection,
    pub(crate) manager: AccountsProxy<'static>,
}

impl Daemon {
    pub(crate) async fn connect() -> anyhow::Result<Self> {
        let connection = zbus::Connection::session().await.context("cannot connect to the session bus")?;
        let manager = AccountsProxy::new(&connection).await?;
        Ok(Self { connection, manager })
    }

    pub(crate) async fn account(&self, path: &OwnedObjectPath) -> zbus::Result<AccountProxy<'static>> {
        AccountProxy::new(&self.connection, path.clone()).await
    }

    pub(crate) async fn sync(&self, path: &OwnedObjectPath) -> zbus::Result<FolderProxies<'static>> {
        FolderProxies::new(&self.connection, path.clone()).await
    }

    /// Every account, in the order they were added; one removed while this runs is left out.
    pub(crate) async fn accounts(&self) -> anyhow::Result<Vec<AccountInfo>> {
        let mut accounts = Vec::new();
        for path in self.manager.list().await? {
            let read = async {
                let account = self.account(&path).await?;
                zbus::Result::Ok(AccountInfo {
                    id: account.id().await?,
                    label: account.label().await?,
                    email: account.email().await?,
                    path: path.clone(),
                })
            };
            match read.await {
                Ok(info) => accounts.push(info),
                Err(e) if Refusal::says_gone(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(accounts)
    }

    /// Stops with the daemon's reason when it has no account because it could not load its
    /// configuration (`Accounts.LastError`): "add one" would then be refused too.
    pub(crate) async fn check_loaded(&self, accounts: &[AccountInfo]) -> anyhow::Result<()> {
        if accounts.is_empty() {
            let trouble = self.manager.last_error().await.unwrap_or_default();
            if !trouble.is_empty() {
                bail!("no account is loaded: {trouble}");
            }
        }
        Ok(())
    }

    /// The account a command acts on (`docs/design/desktop.md` §3): the one `--account` names, else the one
    /// `KONEDRIVE_ACCOUNT` names, else the only account there is.
    pub(crate) async fn chosen(&self, option: Option<&str>) -> anyhow::Result<Chosen> {
        let accounts = self.accounts().await?;
        self.check_loaded(&accounts).await?;
        let account = choice::choose(&accounts, wanted(option))?.clone();
        Ok(Chosen { account, several: accounts.len() > 1 })
    }

    /// The accounts `status` and `sync status` show: the chosen one when one is named, every
    /// account otherwise.
    pub(crate) async fn shown(&self, option: Option<&str>) -> anyhow::Result<Shown> {
        if wanted(option).is_some() {
            let chosen = self.chosen(option).await?;
            return Ok(Shown { accounts: vec![chosen.account], alone: true, several: chosen.several });
        }
        let accounts = self.accounts().await?;
        let (alone, several) = (accounts.len() == 1, accounts.len() > 1);
        Ok(Shown { accounts, alone, several })
    }

    /// Every registered folder, with its account's label and proxies: what a path command's
    /// refusal is explained against.
    pub(crate) async fn folders(&self) -> anyhow::Result<Folders> {
        let (mut folders, mut accounts) = (Vec::new(), 0);
        for path in self.manager.list().await? {
            let read = async {
                let sync = self.sync(&path).await?;
                let root = sync.folder.path().await?;
                let label = self.account(&path).await?.label().await?;
                zbus::Result::Ok(Folder { root, label, sync })
            };
            match read.await {
                Ok(folder) => {
                    accounts += 1;
                    if !folder.root.is_empty() {
                        folders.push(folder);
                    }
                }
                Err(e) if Refusal::says_gone(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(Folders { folders, accounts })
    }
}

/// The accounts `status` and `sync status` show.
pub(crate) struct Shown {
    pub(crate) accounts: Vec<AccountInfo>,
    /// One account, shown on its own: the chosen one, or the only one there is.
    pub(crate) alone: bool,
    /// Whether there are several accounts.
    pub(crate) several: bool,
}

impl Shown {
    /// How a command suggested about `account` starts.
    pub(crate) fn prefix(&self, account: &AccountInfo) -> String {
        choice::command_prefix(Some(&account.label), self.several, variable_set())
    }
}

/// The account a command acts on, and whether there are others.
pub(crate) struct Chosen {
    pub(crate) account: AccountInfo,
    pub(crate) several: bool,
}

impl Chosen {
    /// How a command suggested about this account starts (`choice::command_prefix`).
    pub(crate) fn prefix(&self) -> String {
        choice::command_prefix(Some(&self.account.label), self.several, variable_set())
    }

    /// What a success line starts with: the account's label when there are several.
    pub(crate) fn tag(&self) -> String {
        if self.several {
            format!("{}: ", self.account.label)
        } else {
            String::new()
        }
    }
}

/// Whether `KONEDRIVE_ACCOUNT` is set, and not empty.
pub(crate) fn variable_set() -> bool {
    wanted(None).is_some()
}

/// The name of the account to use, and where it came from: `--account`, else
/// `KONEDRIVE_ACCOUNT` when it is set and not empty.
pub(crate) fn wanted(option: Option<&str>) -> Option<(&str, Source)> {
    static VARIABLE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let variable = VARIABLE.get_or_init(|| std::env::var(ACCOUNT_VARIABLE).ok().filter(|v| !v.trim().is_empty()));
    match (option, variable) {
        (Some(name), _) => Some((name, Source::Option)),
        (None, Some(name)) => Some((name.as_str(), Source::Environment)),
        (None, None) => None,
    }
}

/// One account's registered folder.
pub(crate) struct Folder {
    pub(crate) root: String,
    pub(crate) label: String,
    pub(crate) sync: FolderProxies<'static>,
}

/// Every account's registered folder.
#[derive(Default)]
pub(crate) struct Folders {
    pub(crate) folders: Vec<Folder>,
    /// How many accounts there are, with a folder or without.
    pub(crate) accounts: usize,
}

impl Folders {
    /// The folder that holds `path`, by the rule `Files` routes by: the one that is a
    /// component prefix of it (the CLI has already resolved its directory part).
    pub(crate) fn holder(&self, path: &str) -> Option<&Folder> {
        self.folders.iter().find(|f| Path::new(path).starts_with(&f.root))
    }

    /// Every folder's path.
    pub(crate) fn roots(&self) -> Vec<String> {
        self.folders.iter().map(|f| f.root.clone()).collect()
    }

    /// How a command suggested about `holder` starts: it names the account of the folder
    /// that holds the path; about a path in none, `<account>` stands in.
    pub(crate) fn prefix(&self, holder: Option<&Folder>) -> String {
        choice::command_prefix(holder.map(|f| f.label.as_str()), self.accounts > 1, variable_set())
    }
}
