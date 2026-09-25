use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use clap::{Parser, Subcommand};
use konedrive_dbus::accounts::{Account1Proxy, Accounts1Proxy, Dev1Proxy, Files1Proxy, Sync1Proxy};
use konedrivectl::{AccountAction, AccountInfo, AccountRow, Source, SyncAction, ACCOUNT_VARIABLE, FIRST_LABEL};
use zbus::zvariant::OwnedObjectPath;

#[derive(Parser)]
#[command(
    name = "konedrivectl",
    version,
    about = "Control the KOneDrive daemon",
    after_help = "Choosing the account: a command that acts on one account uses the one --account names, \
                  else the one KONEDRIVE_ACCOUNT names, else the only account there is. With several \
                  accounts and none named, or a name that fits more than one, it stops and lists them. \
                  `status` and `sync status` show every account when none is named. The commands that take \
                  a path (`sync hydrate`, `dehydrate`, `state`, `pin`, `unpin`, `free`) act on the account \
                  whose folder holds the path; they, `account list`, `account add`, `account rename`, \
                  `account remove` and `set-client-id` refuse --account and ignore KONEDRIVE_ACCOUNT."
)]
struct Cli {
    /// The account to act on: its id, its label or its email, as `account list` shows them
    /// (label and email in any case). Without it, KONEDRIVE_ACCOUNT; without that, the only
    /// account there is
    #[arg(long, global = true, value_name = "ACCOUNT", allow_hyphen_values = true)]
    account: Option<String>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List, add, rename and remove accounts
    Account {
        #[command(subcommand)]
        command: AccountCmd,
    },
    /// Save the Application (client) ID of your Microsoft Entra app registration, which
    /// every account signs in with
    ///
    /// Refused while any account is signed in or signing in.
    SetClientId { id: String },
    /// Sign the account in with its Microsoft account, in the browser
    ///
    /// With no account at all, first adds one called Personal. The browser is opened with
    /// xdg-open; the sign-in page's address is printed too.
    Login,
    /// Sign the account out and delete its stored token
    Logout,
    /// Show the account's sign-in state; every account's when there are several and none is
    /// chosen
    Status,
    /// Work with an account's sync folder
    Sync {
        #[command(subcommand)]
        command: SyncCmd,
    },
    /// Development tools
    Dev {
        #[command(subcommand)]
        command: DevCmd,
    },
}

#[derive(Subcommand)]
enum AccountCmd {
    /// List every account: id, label, email, sign-in state, mode, and folder with its state
    List,
    /// Add an account, signed out and with no folder yet, and print its id
    ///
    /// A label has 1 to 40 characters, no "/" and no "@", is not 12 hexadecimal digits (the
    /// shape of an id), and is not another account's label, whatever the case. Then sign it
    /// in: `konedrivectl --account <label> login`.
    Add {
        #[arg(allow_hyphen_values = true)]
        label: String,
    },
    /// Give an account a new label
    Rename {
        /// The account: its id, label or email
        // Not `account`: that id is the global `--account`'s.
        #[arg(value_name = "ACCOUNT", allow_hyphen_values = true)]
        named: String,
        /// The new label
        #[arg(allow_hyphen_values = true)]
        label: String,
    },
    /// Remove an account: forget its folder, sign it out, delete its token and cached data
    ///
    /// Asks nothing. Deleted: the refresh token, the cached name and quota, the list of
    /// OneDrive items, the activity and the conflicts list. Kept: the folder's files, as they
    /// are (a file that was never downloaded stays as an empty placeholder, which reads as
    /// zeros), and rescued files. Refused, changing nothing, while the folder needs the helper
    /// to be forgotten and the helper is not connected.
    Remove {
        /// The account: its id, label or email
        #[arg(value_name = "ACCOUNT", allow_hyphen_values = true)]
        named: String,
    },
    /// Show the account's mode, or switch it to read-only or read-write
    ///
    /// read-write signs in again, in the browser as `login` does, asking Microsoft for
    /// permission to change the account's files, and waits; nothing changes until that
    /// permission is granted. While uploads are being developed, only the test accounts listed
    /// in write_test_drive_ids in config.toml can be read-write. read-only needs no sign-in,
    /// and is refused while changes wait to be uploaded, unless --force.
    Mode {
        /// read-only or read-write; without it, the mode is shown
        #[arg(value_parser = ["read-only", "read-write"])]
        mode: Option<String>,
        /// Switch to read-only even while changes wait to be uploaded: they stay here, and
        /// are not uploaded
        #[arg(long, requires = "mode")]
        force: bool,
    },
}

#[derive(Subcommand)]
enum DevCmd {
    /// Write an access token of the account — about an hour of read access, never the
    /// refresh token — to a file only you can read, for a test run in the VM
    ExportAccessToken {
        #[arg(long)]
        out: std::path::PathBuf,
        /// A token that can change the account's files, for the test-account harness: only
        /// for a read-write account listed in write_test_drive_ids in config.toml
        #[arg(long)]
        read_write: bool,
    },
}

#[derive(Subcommand)]
enum SyncCmd {
    /// Bind an empty folder to the account
    Register { path: String },
    /// The developer's mode: bind a local folder with NOTHING intercepting opens inside it
    ///
    /// The folder is filled from a directory with `populate-from`. Without
    /// the helper, files that are not downloaded read as zeros until you
    /// `hydrate` them by hand. It never shows OneDrive: that takes `register`
    /// and the helper. Named after the D-Bus method it calls
    /// (`RegisterRootWithoutInterception`) rather than something shorter, on
    /// purpose: the cost this mode carries belongs in the word you type, not
    /// just in a warning you might scroll past.
    RegisterWithoutInterception { path: String },
    /// Forget the account's folder (local files are left as they are)
    Forget,
    /// Fill the account's folder with placeholders mirroring a local directory
    PopulateFrom { source_dir: String },
    /// Download one file now. The path decides the account
    Hydrate { path: String },
    /// Free up space for one file. The path decides the account
    Dehydrate { path: String },
    /// Print one file's state. The path decides the account
    State { path: String },
    /// Show the account's folder and its state; every account's when there are several and
    /// none is chosen
    Status,
    /// List what is in OneDrive but not in the folder, and why
    Skipped,
    /// Ask OneDrive for changes now
    Refresh,
    /// Show what happened lately, newest first: downloads, free-ups, changes
    /// from OneDrive, conflicts, failures
    Activity {
        /// How many events to show (the daemon keeps the last 200)
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Show the downloads and uploads under way
    Transfers,
    /// Show the changes waiting to be uploaded, and why each waits
    Outbox {
        /// Show every change, not only the first 50
        #[arg(long)]
        all: bool,
    },
    /// Pause syncing: nothing is uploaded and OneDrive is not asked for changes.
    /// Opening a file still downloads it
    Pause {
        /// How long: `30m`, `2h`, `1d`, `1h30m`; until `sync resume` without it
        #[arg(long = "for", value_name = "DURATION")]
        duration: Option<String>,
    },
    /// Resume syncing now
    Resume,
    /// Show or change the names of local files that are never uploaded (shell
    /// globs, matched against a name)
    Ignore {
        #[command(subcommand)]
        action: Option<IgnoreCmd>,
    },
    /// List what stays on this computer, and why
    NotUploaded,
    /// Decide on a large delete held for confirmation
    Deletes {
        #[command(subcommand)]
        action: DeletesCmd,
    },
    /// List your changed versions that were kept when the file changed or was
    /// removed in OneDrive: moved out of the way, or kept as a copy beside it
    Conflicts,
    /// Take a conflict off the list; the file itself stays where it is
    Dismiss {
        /// Where your version is kept, as `sync conflicts` shows it
        path: String,
    },
    /// Free up the space of every downloaded file in the account's folder that is not in use
    FreeUpSpace,
    /// Always keep files or folders on this device: everything in them is
    /// downloaded now, and whatever comes into a folder later. The paths decide the
    /// accounts
    Pin {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Stop always keeping files or folders on this device; what is
    /// downloaded stays downloaded. The paths decide the accounts
    Unpin {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Free up space for files or folders: a file or folder you pinned stops
    /// being kept on this device, and everything in it is freed up. The paths decide the
    /// accounts
    Free {
        #[arg(required = true)]
        paths: Vec<String>,
    },
}

#[derive(Subcommand)]
enum IgnoreCmd {
    /// Print the list
    List,
    /// Add a pattern, such as `*.bak`
    Add { pattern: String },
    /// Remove a pattern
    Remove { pattern: String },
}

#[derive(Subcommand)]
enum DeletesCmd {
    /// Delete them in OneDrive too
    Confirm,
    /// Keep them in OneDrive: they come back here
    Restore,
}

/// A command line that has to change: exits with status 2, as clap's own usage errors do.
#[derive(Debug)]
struct Usage(String);

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Usage {}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:?}");
            if error.downcast_ref::<Usage>().is_some() {
                ExitCode::from(2)
            } else if let Some(no_choice) = error.downcast_ref::<konedrivectl::NoChoice>() {
                ExitCode::from(no_choice.exit_status())
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

/// Why `command` takes no `--account`, if it takes none: it names its account itself, acts on
/// every account, or (a path command) acts on the account whose folder holds the path. Given
/// anyway, `--account` is refused rather than ignored, so a mistaken option never silently
/// does something else. `KONEDRIVE_ACCOUNT`, a default for a whole shell, is ignored by them.
fn takes_no_account(command: &Cmd) -> Option<&'static str> {
    match command {
        Cmd::SetClientId { .. } => Some("the client ID is one for every account"),
        Cmd::Account { command: AccountCmd::List } => Some("`account list` shows every account"),
        Cmd::Account { command: AccountCmd::Add { .. } } => Some("`account add` adds a new account"),
        Cmd::Account { command: AccountCmd::Rename { .. } | AccountCmd::Remove { .. } } => {
            Some("`account rename` and `account remove` take the account as their first argument")
        }
        Cmd::Sync {
            command:
                SyncCmd::Hydrate { .. }
                | SyncCmd::Dehydrate { .. }
                | SyncCmd::State { .. }
                | SyncCmd::Pin { .. }
                | SyncCmd::Unpin { .. }
                | SyncCmd::Free { .. },
        } => Some("the path decides the account"),
        _ => None,
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    if let (Some(_), Some(why)) = (&cli.account, takes_no_account(&cli.command)) {
        return Err(Usage(format!("{why}: leave out --account")).into());
    }
    let daemon = Daemon::connect().await?;
    let option = cli.account.as_deref();
    match cli.command {
        Cmd::Account { command } => account(&daemon, option, command).await,
        Cmd::SetClientId { id } => set_client_id(&daemon, &id).await,
        Cmd::Login => login(&daemon, option).await,
        Cmd::Logout => {
            let chosen = daemon.chosen(option).await?.account;
            let proxy = daemon.account(&chosen.path).await?;
            let result = proxy.sign_out().await;
            result.map_err(|e| anyhow!(konedrivectl::explain_account_error(AccountAction::SignOut(&chosen.label), &e)))?;
            println!("Signed out of {}.", chosen.label);
            Ok(())
        }
        Cmd::Status => status(&daemon, option).await,
        Cmd::Sync { command } => sync(&daemon, option, command).await,
        Cmd::Dev { command } => dev(&daemon, option, command).await,
    }
}

/// The daemon: the accounts manager, and each account's objects.
struct Daemon {
    connection: zbus::Connection,
    manager: Accounts1Proxy<'static>,
}

impl Daemon {
    async fn connect() -> anyhow::Result<Self> {
        let connection = zbus::Connection::session().await.context("cannot connect to the session bus")?;
        let manager = Accounts1Proxy::new(&connection).await?;
        Ok(Self { connection, manager })
    }

    async fn account(&self, path: &OwnedObjectPath) -> zbus::Result<Account1Proxy<'static>> {
        Account1Proxy::new(&self.connection, path.clone()).await
    }

    async fn sync(&self, path: &OwnedObjectPath) -> zbus::Result<Sync1Proxy<'static>> {
        Sync1Proxy::new(&self.connection, path.clone()).await
    }

    /// Every account, in the order they were added; one removed while this runs is left out.
    async fn accounts(&self) -> anyhow::Result<Vec<AccountInfo>> {
        let mut accounts = Vec::new();
        for path in self.manager.accounts().await? {
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
                Err(e) if konedrivectl::is_gone(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(accounts)
    }

    /// Stops with the daemon's reason when it has no account because it could not load its
    /// configuration (`Accounts1.LastError`): "add one" would then be refused too.
    async fn check_loaded(&self, accounts: &[AccountInfo]) -> anyhow::Result<()> {
        if accounts.is_empty() {
            let trouble = self.manager.last_error().await.unwrap_or_default();
            if !trouble.is_empty() {
                bail!("no account is loaded: {trouble}");
            }
        }
        Ok(())
    }

    /// The account a command acts on (design §5.1): the one `--account` names, else the one
    /// `KONEDRIVE_ACCOUNT` names, else the only account there is.
    async fn chosen(&self, option: Option<&str>) -> anyhow::Result<Chosen> {
        let accounts = self.accounts().await?;
        self.check_loaded(&accounts).await?;
        let account = konedrivectl::choose(&accounts, wanted(option))?.clone();
        Ok(Chosen { account, several: accounts.len() > 1 })
    }

    /// The accounts `status` and `sync status` show: the chosen one when one is named, every
    /// account otherwise; whether that is one account shown on its own; and whether there
    /// are several.
    async fn shown(&self, option: Option<&str>) -> anyhow::Result<(Vec<AccountInfo>, bool, bool)> {
        if wanted(option).is_some() {
            let chosen = self.chosen(option).await?;
            return Ok((vec![chosen.account], true, chosen.several));
        }
        let accounts = self.accounts().await?;
        let (alone, several) = (accounts.len() == 1, accounts.len() > 1);
        Ok((accounts, alone, several))
    }

    /// Every registered folder, with its account's label and `Sync1`: what a path command's
    /// refusal is explained against; and how many accounts there are.
    async fn folders(&self) -> anyhow::Result<(Vec<Folder>, usize)> {
        let (mut folders, mut accounts) = (Vec::new(), 0);
        for path in self.manager.accounts().await? {
            let read = async {
                let sync = self.sync(&path).await?;
                let root = sync.root_path().await?;
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
                Err(e) if konedrivectl::is_gone(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok((folders, accounts))
    }
}

/// The account a command acts on, and whether there are others.
struct Chosen {
    account: AccountInfo,
    several: bool,
}

impl Chosen {
    /// How a command suggested about this account starts (`konedrivectl::command_prefix`).
    fn prefix(&self) -> String {
        konedrivectl::command_prefix(Some(&self.account.label), self.several, variable_set())
    }

    /// What a success line starts with: the account's label when there are several.
    fn tag(&self) -> String {
        if self.several {
            format!("{}: ", self.account.label)
        } else {
            String::new()
        }
    }
}

/// Whether `KONEDRIVE_ACCOUNT` is set, and not empty.
fn variable_set() -> bool {
    wanted(None).is_some()
}

/// The name of the account to use, and where it came from: `--account`, else
/// `KONEDRIVE_ACCOUNT` when it is set and not empty.
fn wanted(option: Option<&str>) -> Option<(&str, Source)> {
    static VARIABLE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let variable = VARIABLE.get_or_init(|| std::env::var(ACCOUNT_VARIABLE).ok().filter(|v| !v.trim().is_empty()));
    match (option, variable) {
        (Some(name), _) => Some((name, Source::Option)),
        (None, Some(name)) => Some((name.as_str(), Source::Environment)),
        (None, None) => None,
    }
}

/// One account's registered folder.
struct Folder {
    root: String,
    label: String,
    sync: Sync1Proxy<'static>,
}

/// The folder that holds `path`, by the rule `Files1` routes by: the one that is a
/// component prefix of it (the CLI has already resolved its directory part).
fn holder<'f>(folders: &'f [Folder], path: &str) -> Option<&'f Folder> {
    folders.iter().find(|f| Path::new(path).starts_with(&f.root))
}

async fn account(daemon: &Daemon, option: Option<&str>, command: AccountCmd) -> anyhow::Result<()> {
    match command {
        AccountCmd::Mode { mode, force } => return account_mode(daemon, option, mode.as_deref(), force).await,
        AccountCmd::List => {
            let mut rows = Vec::new();
            for path in daemon.manager.accounts().await? {
                let read = async {
                    let (account, sync) = (daemon.account(&path).await?, daemon.sync(&path).await?);
                    zbus::Result::Ok(AccountRow {
                        id: account.id().await?,
                        label: account.label().await?,
                        email: account.email().await?,
                        state: account.state().await?,
                        mode: account.mode().await?,
                        folder: sync.root_path().await?,
                        root_state: sync.root_state().await?,
                    })
                };
                match read.await {
                    Ok(row) => rows.push(row),
                    // Removed while this ran.
                    Err(e) if konedrivectl::is_gone(&e) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            print!("{}", konedrivectl::account_list_text(&rows));
            let trouble = daemon.manager.last_error().await?;
            if !trouble.is_empty() {
                eprintln!("warning: {trouble}");
            }
        }
        AccountCmd::Add { label } => {
            let result = daemon.manager.add(&label).await;
            let path = result.map_err(|e| anyhow!(konedrivectl::explain_account_error(AccountAction::Add(&label), &e)))?;
            let added = daemon.account(&path).await?;
            let (id, label) = (added.id().await?, added.label().await?);
            println!("Added the account {label} ({id}), signed out and with no folder yet.");
            println!("Sign it in with: konedrivectl --account {} login", konedrivectl::shell_word(&label));
        }
        AccountCmd::Rename { named, label } => {
            let accounts = daemon.accounts().await?;
            daemon.check_loaded(&accounts).await?;
            let target = konedrivectl::choose(&accounts, Some((&named, Source::Argument)))?;
            let proxy = daemon.account(&target.path).await?;
            let result = proxy.set_label(&label).await;
            let action = AccountAction::Rename(&target.label, &label);
            result.map_err(|e| anyhow!(konedrivectl::explain_account_error(action, &e)))?;
            println!("Renamed {} to {}.", target.label, label.trim());
        }
        AccountCmd::Remove { named } => {
            let accounts = daemon.accounts().await?;
            daemon.check_loaded(&accounts).await?;
            let target = konedrivectl::choose(&accounts, Some((&named, Source::Argument)))?;
            let sync = daemon.sync(&target.path).await?;
            let folder = sync.root_path().await.unwrap_or_default();
            // Read first: the list goes with the account. Where each listed file was rescued
            // to is the one thing about rescues this can know (F51).
            let conflicts = sync.conflicts().await.unwrap_or_default();
            let result = daemon.manager.remove(&target.path.as_ref()).await;
            if let Err(error) = result {
                let helper = daemon.manager.helper_state().await.unwrap_or_default();
                let source = sync.root_source().await.unwrap_or_default();
                let context = konedrivectl::Context { root: &folder, source: &source, helper: &helper, ..Default::default() };
                let action = SyncAction::Remove(&target.label);
                bail!("{}", konedrivectl::explain_sync_error_in(action, &error, context));
            }
            print!("{}", konedrivectl::removed_text(&target.label, &folder, &conflicts));
        }
    }
    Ok(())
}

/// `account mode` (`docs/design/writes.md` §11): the chosen account's mode, or a switch. A switch to
/// read-write opens the sign-in the daemon answers with, as `login` does, and waits until the
/// account is read-write or says why it is not.
async fn account_mode(daemon: &Daemon, option: Option<&str>, mode: Option<&str>, force: bool) -> anyhow::Result<()> {
    let chosen = daemon.chosen(option).await?;
    let (label, tag, prefix) = (chosen.account.label.clone(), chosen.tag(), chosen.prefix());
    let proxy = daemon.account(&chosen.account.path).await?;
    let Some(mode) = mode else {
        println!("{tag}{}", proxy.mode().await?);
        let last_error = proxy.last_error().await?;
        if !last_error.is_empty() {
            println!("{:<12}{last_error}", "Last error:");
        }
        return Ok(());
    };
    let result = proxy.set_mode(mode, force).await;
    let url = result.map_err(|e| anyhow!(konedrivectl::explain_account_error(AccountAction::SetMode(&label, mode, &prefix), &e)))?;
    if mode == "read-only" {
        println!("{tag}Read-only: the folder's files are read-only again, and nothing changed in it is uploaded.");
        return Ok(());
    }
    if url.is_empty() {
        println!("{tag}Already read-write.");
        return Ok(());
    }
    println!(
        "Opening the Microsoft sign-in page in your browser, to allow konedrive to change the files \
         of {label} in OneDrive. If it does not open, visit:\n\n  {url}\n"
    );
    let _ = Command::new("xdg-open").arg(&url).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    // Uncached, as `login`'s: the wait polls `Mode` directly.
    let wait_proxy = Account1Proxy::builder(&daemon.connection)
        .path(chosen.account.path.clone())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(6 * 60), konedrivectl::wait_for_read_write(&wait_proxy)) => {
            result.context("timed out")??;
        }
        _ = tokio::signal::ctrl_c() => {
            proxy.cancel_sign_in().await?;
            bail!("cancelled; {label} stays read-only");
        }
    }
    println!("{tag}Read-write: the folder's files can be changed.");
    Ok(())
}

async fn set_client_id(daemon: &Daemon, id: &str) -> anyhow::Result<()> {
    if let Err(error) = daemon.manager.set_client_id(id).await {
        // The daemon refuses while any account uses the old id: name them.
        let mut busy = Vec::new();
        for account in daemon.accounts().await.unwrap_or_default() {
            let state = match daemon.account(&account.path).await {
                Ok(proxy) => proxy.state().await.unwrap_or_default(),
                Err(_) => continue,
            };
            if state == "signed-in" || state == "signing-in" {
                busy.push(account.label);
            }
        }
        bail!("{}", konedrivectl::explain_account_error(AccountAction::SetClientId(id, &busy), &error));
    }
    println!("Client ID saved.");
    Ok(())
}

async fn status(daemon: &Daemon, option: Option<&str>) -> anyhow::Result<()> {
    let client_id = daemon.manager.client_id().await?;
    let trouble = daemon.manager.last_error().await?;
    let trouble = if trouble.is_empty() { String::new() } else { format!("{:<12}{trouble}\n", "Problem:") };
    let (accounts, alone, _) = daemon.shown(option).await?;
    if alone {
        let proxy = daemon.account(&accounts[0].path).await?;
        print!("{}{trouble}", konedrivectl::status_text(&proxy, Some(&client_id)).await?);
        return Ok(());
    }
    let mut out = format!("{}{trouble}", konedrivectl::client_id_line(&client_id));
    if accounts.is_empty() {
        out.push_str(&format!(
            "{:<12}none yet: `konedrivectl login` adds one called {FIRST_LABEL} and signs it in\n",
            "Accounts:"
        ));
    }
    for account in &accounts {
        let read = async { konedrivectl::status_text(&daemon.account(&account.path).await?, None).await };
        match read.await {
            Ok(block) => out.push_str(&format!("\n{}\n{}", account.label, konedrivectl::indented(&block))),
            // Removed while this ran.
            Err(e) if konedrivectl::is_gone(&e) => {}
            Err(e) => return Err(e.into()),
        }
    }
    print!("{out}");
    Ok(())
}

async fn sync_status(daemon: &Daemon, option: Option<&str>) -> anyhow::Result<()> {
    let helper = daemon.manager.helper_state().await?;
    let (accounts, alone, several) = daemon.shown(option).await?;
    let variable = variable_set();
    if alone {
        let sync = daemon.sync(&accounts[0].path).await?;
        let prefix = konedrivectl::command_prefix(Some(&accounts[0].label), several, variable);
        print!("{}", konedrivectl::sync_status_text(&sync, Some(&helper), &prefix).await?);
        return Ok(());
    }
    daemon.check_loaded(&accounts).await?;
    let mut out = konedrivectl::helper_line(&helper);
    if accounts.is_empty() {
        out.push_str(&format!("{}\n", konedrivectl::NoChoice::NoAccountYet));
    }
    for account in &accounts {
        let prefix = konedrivectl::command_prefix(Some(&account.label), several, variable);
        let read = async { konedrivectl::sync_status_text(&daemon.sync(&account.path).await?, None, &prefix).await };
        match read.await {
            Ok(block) => out.push_str(&format!("\n{}\n{}", account.label, konedrivectl::indented(&block))),
            // Removed while this ran.
            Err(e) if konedrivectl::is_gone(&e) => {}
            Err(e) => return Err(e.into()),
        }
    }
    print!("{out}");
    Ok(())
}

async fn dev(daemon: &Daemon, option: Option<&str>, command: DevCmd) -> anyhow::Result<()> {
    match command {
        DevCmd::ExportAccessToken { out, read_write } => {
            let chosen = daemon.chosen(option).await?;
            let dev = Dev1Proxy::new(&daemon.connection, chosen.account.path.clone()).await?;
            // Read-only unless asked; the daemon refuses a read-write token for any account
            // the development gate does not let through (`docs/design/writes.md` §8.2; SECURITY.md).
            let token = if read_write { dev.read_write_access_token().await } else { dev.access_token().await };
            let token = token.map_err(|e| anyhow!("{}", konedrivectl::explain_dev_error(&e, &chosen.prefix())))?;
            // I1: `write_secret_atomically` never opens `out`
            // itself, so a symlink there is replaced rather than followed
            // and truncated, and anyone who already had the old file open
            // keeps reading its old content undisturbed.
            konedrivectl::write_secret_atomically(&out, token.as_bytes())
                .with_context(|| format!("cannot write the access token to {}", out.display()))?;
            let what = if read_write { "that can CHANGE its files in OneDrive" } else { "that can only read" };
            println!(
                "Wrote an access token of {} {what}, valid for about an hour, to {}. It is not the \
                 refresh token. Delete the file when the test is done.",
                chosen.account.label,
                out.display()
            );
        }
    }
    Ok(())
}

async fn sync(daemon: &Daemon, option: Option<&str>, command: SyncCmd) -> anyhow::Result<()> {
    match command {
        SyncCmd::Status => return sync_status(daemon, option).await,
        SyncCmd::Hydrate { path } => {
            let absolute = absolute_str(&path)?;
            let files = Files1Proxy::new(&daemon.connection).await?;
            let paths = [absolute.clone()];
            let result = files.hydrate(&absolute).await;
            explained_paths(daemon, PathAction::Hydrate, &paths, result).await?;
            println!("Downloaded.");
            fail_if_holders_unhealthy(daemon, &paths).await?;
        }
        SyncCmd::Dehydrate { path } => {
            let absolute = absolute_str(&path)?;
            let files = Files1Proxy::new(&daemon.connection).await?;
            let paths = [absolute.clone()];
            let result = files.dehydrate(&absolute).await;
            explained_paths(daemon, PathAction::Dehydrate, &paths, result).await?;
            println!("Freed up.");
            fail_if_holders_unhealthy(daemon, &paths).await?;
        }
        SyncCmd::State { path } => {
            let files = Files1Proxy::new(&daemon.connection).await?;
            println!("{}", files.item_state(&absolute_str(&path)?).await?);
        }
        SyncCmd::Pin { paths } => {
            let absolute = absolute_all(&paths)?;
            let refs: Vec<&str> = absolute.iter().map(String::as_str).collect();
            let files = Files1Proxy::new(&daemon.connection).await?;
            let result = files.pin(&refs).await;
            let queued = explained_paths(daemon, PathAction::Pin, &absolute, result).await?;
            // `sync transfers` is one account's: the one the paths are in, if they are in one.
            let (folders, accounts) = daemon.folders().await?;
            let mut labels: Vec<&str> =
                absolute.iter().filter_map(|path| holder(&folders, path)).map(|f| f.label.as_str()).collect();
            labels.dedup();
            let label = match labels.as_slice() {
                [one] => Some(*one),
                _ => None,
            };
            let prefix = konedrivectl::command_prefix(label, accounts > 1, variable_set());
            println!("{}", konedrivectl::pin_text(queued, &prefix));
            fail_if_holders_unhealthy(daemon, &absolute).await?;
        }
        SyncCmd::Unpin { paths } => {
            let absolute = absolute_all(&paths)?;
            let refs: Vec<&str> = absolute.iter().map(String::as_str).collect();
            let files = Files1Proxy::new(&daemon.connection).await?;
            let result = files.unpin(&refs).await;
            let unpinned = explained_paths(daemon, PathAction::Unpin, &absolute, result).await?;
            println!("{}", konedrivectl::unpin_text(unpinned));
            fail_if_holders_unhealthy(daemon, &absolute).await?;
        }
        SyncCmd::Free { paths } => {
            let absolute = absolute_all(&paths)?;
            let refs: Vec<&str> = absolute.iter().map(String::as_str).collect();
            let files = Files1Proxy::new(&daemon.connection).await?;
            let result = files.free_up(&refs).await;
            let (freed, bytes, busy, pinned) = explained_paths(daemon, PathAction::Free, &absolute, result).await?;
            println!("{}", konedrivectl::free_text(freed, bytes, busy, pinned));
            fail_if_holders_unhealthy(daemon, &absolute).await?;
        }
        command => {
            let chosen = daemon.chosen(option).await?;
            let proxy = daemon.sync(&chosen.account.path).await?;
            folder_command(daemon, &chosen, &proxy, command).await?;
        }
    }
    Ok(())
}

/// The `sync` commands that act on the chosen account's folder, through its `Sync1`. With
/// several accounts, each success line starts with the account's label.
async fn folder_command(daemon: &Daemon, chosen: &Chosen, proxy: &Sync1Proxy<'_>, command: SyncCmd) -> anyhow::Result<()> {
    let tag = chosen.tag();
    match command {
        SyncCmd::Register { path } => {
            let absolute = absolute_str(&path)?;
            let action = SyncAction::Register(&absolute);
            explained(daemon, chosen, proxy, action, proxy.register_root(&absolute).await).await?;
            match root_trouble(proxy).await? {
                None => println!("{tag}Folder registered: {absolute}"),
                // `SyncService::register_root`'s own doc comment says the
                // call still returns `Ok(())` here (the root itself is
                // usable) — but printing "registered" with nothing else
                // would be C1: the one thing this interface exists to make
                // visible, silently. This is a hard failure (bail, non-zero
                // exit, no bare "registered" line) rather than a warning
                // alongside a success line, because a script checking only
                // the exit status must see the same trouble a human reading
                // stdout would.
                Some(detail) => bail!(
                    "the folder at {absolute} is registered, but was not fully recovered: {detail}"
                ),
            }
        }
        SyncCmd::RegisterWithoutInterception { path } => {
            let absolute = absolute_str(&path)?;
            let action = SyncAction::RegisterWithoutInterception(&absolute);
            let result = proxy.register_root_without_interception(&absolute).await;
            explained(daemon, chosen, proxy, action, result).await?;
            match root_trouble(proxy).await? {
                Some(detail) => bail!(
                    "the folder at {absolute} is registered, but was not fully recovered: {detail}"
                ),
                None => {
                    println!("{tag}Folder registered without interception: {absolute}");
                    // Always shown, success or not: the entire point of this
                    // mode is that a placeholder nobody intercepts reads as
                    // zeros, and that must never be left to be inferred.
                    let last_error = proxy.last_error().await?;
                    if !last_error.is_empty() {
                        eprintln!("warning: {last_error}");
                    }
                }
            }
        }
        SyncCmd::Forget => {
            explained(daemon, chosen, proxy, SyncAction::Forget, proxy.unregister_root().await).await?;
            println!("{tag}Folder forgotten. Local files were left untouched.");
            fail_if_root_unhealthy(proxy).await?;
        }
        SyncCmd::PopulateFrom { source_dir } => {
            let absolute = absolute_str(&source_dir)?;
            let action = SyncAction::PopulateFrom(&absolute);
            let created = explained(daemon, chosen, proxy, action, proxy.populate_from_directory(&absolute).await).await?;
            println!("{tag}Created {created} placeholders.");
            fail_if_root_unhealthy(proxy).await?;
        }
        SyncCmd::Skipped => {
            let root_path = proxy.root_path().await?;
            if root_path.is_empty() {
                println!("No folder is registered.");
            } else if proxy.root_source().await? != "onedrive" {
                println!("This folder is not connected to OneDrive.");
            } else {
                // Read before `Skipped()` itself: a listing that finishes in
                // between just means the list this call gets back is a
                // little more complete than the note says, never less.
                let still_listing = proxy.root_state().await? == "listing";
                let skipped = explained(daemon, chosen, proxy, SyncAction::Skipped, proxy.skipped().await).await?;
                if still_listing {
                    println!(
                        "The folder is still being filled from OneDrive; this list may be partial."
                    );
                }
                if skipped.is_empty() {
                    println!("Nothing is skipped.");
                }
                for (path, reason) in skipped {
                    println!("{path}\n    {}", konedrivectl::skip_reason_text(&reason));
                }
            }
        }
        SyncCmd::Refresh => {
            explained(daemon, chosen, proxy, SyncAction::Refresh, proxy.refresh().await).await?;
            println!("{tag}Asked OneDrive for changes.");
        }
        SyncCmd::Activity { limit } => {
            let events = explained(daemon, chosen, proxy, SyncAction::Activity, proxy.recent_activity(limit).await).await?;
            print!("{}", konedrivectl::activity_text(&events));
        }
        SyncCmd::Transfers => print!("{}", konedrivectl::transfers_text(&proxy.transfers().await?, &proxy.uploads().await?)),
        SyncCmd::Outbox { all } => {
            const SHOWN: u32 = 50;
            let limit = if all { 0 } else { SHOWN + 1 };
            let mut rows = explained(daemon, chosen, proxy, SyncAction::Outbox, proxy.outbox(limit).await).await?;
            let more = !all && rows.len() > SHOWN as usize;
            rows.truncate(if all { rows.len() } else { SHOWN as usize });
            print!("{}", konedrivectl::outbox_text(&rows, more, &chosen.prefix()));
        }
        SyncCmd::Pause { duration } => {
            let seconds = match duration.as_deref() {
                None => 0,
                Some(text) => konedrivectl::parse_duration(text)
                    .ok_or_else(|| Usage(format!("`{text}` is not a duration: write it as 30m, 2h, 1d or 1h30m")))?,
            };
            explained(daemon, chosen, proxy, SyncAction::Pause, proxy.pause(seconds).await).await?;
            match seconds {
                0 => println!("{tag}Paused until `{} sync resume`.", chosen.prefix()),
                _ => println!("{tag}Paused until {}.", konedrivectl::local_time(proxy.paused_until().await?)),
            }
        }
        SyncCmd::Resume => {
            explained(daemon, chosen, proxy, SyncAction::Resume, proxy.resume().await).await?;
            println!("{tag}Resumed.");
        }
        SyncCmd::Ignore { action } => {
            let patterns = proxy.ignore_patterns().await?;
            let changed = match action {
                None | Some(IgnoreCmd::List) => {
                    for pattern in &patterns {
                        println!("{pattern}");
                    }
                    None
                }
                Some(IgnoreCmd::Add { pattern }) if patterns.contains(&pattern) => {
                    println!("{tag}`{pattern}` is on the list already.");
                    None
                }
                Some(IgnoreCmd::Add { pattern }) => {
                    let mut new = patterns.clone();
                    new.push(pattern.clone());
                    Some((new, format!("{tag}Added `{pattern}`: local files named so are not uploaded.")))
                }
                Some(IgnoreCmd::Remove { pattern }) => {
                    if !patterns.contains(&pattern) {
                        return Err(Usage(format!("`{pattern}` is not on the list (`{} sync ignore list`)", chosen.prefix())).into());
                    }
                    let new: Vec<String> = patterns.iter().filter(|p| **p != pattern).cloned().collect();
                    Some((new, format!("{tag}Removed `{pattern}`: local files named so are uploaded from now on.")))
                }
            };
            if let Some((new, said)) = changed {
                let refs: Vec<&str> = new.iter().map(String::as_str).collect();
                explained(daemon, chosen, proxy, SyncAction::Ignore, proxy.set_ignore_patterns(&refs).await).await?;
                println!("{said}");
            }
        }
        SyncCmd::NotUploaded => {
            let items = explained(daemon, chosen, proxy, SyncAction::NotUploaded, proxy.not_uploaded().await).await?;
            print!("{}", konedrivectl::not_uploaded_text(&items));
        }
        SyncCmd::Deletes { action } => match action {
            DeletesCmd::Confirm => {
                let n = explained(daemon, chosen, proxy, SyncAction::Deletes, proxy.confirm_deletes().await).await?;
                match n {
                    0 => println!("{tag}No delete is waiting for confirmation."),
                    n => println!("{tag}Confirmed: {n} change(s) go to OneDrive's recycle bin."),
                }
            }
            DeletesCmd::Restore => {
                let n = explained(daemon, chosen, proxy, SyncAction::Deletes, proxy.restore_deletes().await).await?;
                match n {
                    0 => println!("{tag}No delete is waiting for confirmation."),
                    n => println!("{tag}Restored: {n} change(s) dropped; the items come back from OneDrive."),
                }
            }
        },
        SyncCmd::Conflicts => {
            let conflicts = explained(daemon, chosen, proxy, SyncAction::Conflicts, proxy.conflicts().await).await?;
            print!("{}", konedrivectl::conflicts_text(&conflicts));
        }
        SyncCmd::Dismiss { path } => {
            // As the daemon recorded it: made absolute, never resolved — the
            // file may be gone, and a link on the way to it must not change
            // which conflict this names.
            let absolute = std::path::absolute(&path).context("cannot make the path absolute")?;
            let absolute = absolute.to_str().context("non-UTF-8 path")?;
            let action = SyncAction::Dismiss(absolute);
            explained(daemon, chosen, proxy, action, proxy.dismiss_conflict(absolute).await).await?;
            println!("{tag}Dismissed. The file was left where it is.");
        }
        SyncCmd::FreeUpSpace => {
            let (files, bytes, busy) =
                explained(daemon, chosen, proxy, SyncAction::FreeUpSpace, proxy.free_up_space().await).await?;
            println!("{tag}{}", konedrivectl::free_up_text(files, bytes, busy));
            if proxy.pinned_count().await? > 0 {
                println!("Files kept on this device (`konedrivectl sync pin`) were left as they are.");
            }
        }
        SyncCmd::Status
        | SyncCmd::Hydrate { .. }
        | SyncCmd::Dehydrate { .. }
        | SyncCmd::State { .. }
        | SyncCmd::Pin { .. }
        | SyncCmd::Unpin { .. }
        | SyncCmd::Free { .. } => unreachable!("handled by `sync`"),
    }
    Ok(())
}

/// [`absolute_str`] for each of `paths`.
fn absolute_all(paths: &[String]) -> anyhow::Result<Vec<String>> {
    paths.iter().map(|path| absolute_str(path)).collect()
}

/// Passes a `Sync1` call's result through, turning a refusal into what the
/// person running this should read (`konedrivectl::explain_sync_error`):
/// matched by the D-Bus error name, and said in terms of their own file.
async fn explained<T>(
    daemon: &Daemon,
    chosen: &Chosen,
    proxy: &Sync1Proxy<'_>,
    action: SyncAction<'_>,
    result: zbus::Result<T>,
) -> anyhow::Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            // Two refusals name the registered folder, one depends on what
            // it shows, and one ends with how to start the helper. If even
            // reading them fails, they simply do not; the refusal is the
            // thing to report.
            let root = proxy.root_path().await.unwrap_or_default();
            let source = proxy.root_source().await.unwrap_or_default();
            let helper = daemon.manager.helper_state().await.unwrap_or_default();
            let foreign = match action {
                SyncAction::Register(path) | SyncAction::RegisterWithoutInterception(path) => carries_a_drive(path),
                _ => false,
            };
            let prefix = chosen.prefix();
            let context = konedrivectl::Context {
                root: &root,
                source: &source,
                helper: &helper,
                folders: &[],
                foreign,
                prefix: &prefix,
            };
            Err(anyhow!("{}", konedrivectl::explain_sync_error_in(action, &error, context)))
        }
    }
}

/// Passes a `Files1` call's result on `paths` through, as [`explained`] does. `Files1` finds
/// the account by the path, so the refusal is explained against the folder that holds the
/// path it is about — the one a `NotAllowed` names, else the first in no folder, else the
/// first given — and, for a path in none, against every account's folder.
async fn explained_paths<T>(
    daemon: &Daemon,
    action: PathAction,
    paths: &[String],
    result: zbus::Result<T>,
) -> anyhow::Result<T> {
    let error = match result {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    let (folders, accounts) = daemon.folders().await.unwrap_or_default();
    let refused = konedrivectl::refused_path(&error).map(str::to_owned).or_else(|| {
        let outside = konedrive_dbus::error_name(&error) == Some("org.konedrive.Error.OutsideRoot");
        paths.iter().find(|path| outside && holder(&folders, path).is_none()).cloned()
    });
    let named = refused.clone().unwrap_or_else(|| paths.join(", "));
    let about = refused.as_deref().or(paths.first().map(String::as_str)).unwrap_or_default();
    let held_by = holder(&folders, about);
    let root = held_by.map(|f| f.root.clone()).unwrap_or_default();
    let source = match held_by {
        Some(folder) => folder.sync.root_source().await.unwrap_or_default(),
        None => String::new(),
    };
    let helper = daemon.manager.helper_state().await.unwrap_or_default();
    let roots: Vec<String> = folders.iter().map(|f| f.root.clone()).collect();
    // A command suggested about the folder that holds the path names its account; about
    // a path in none, `<account>` stands in.
    let prefix = konedrivectl::command_prefix(held_by.map(|f| f.label.as_str()), accounts > 1, variable_set());
    let context = konedrivectl::Context {
        root: &root,
        source: &source,
        helper: &helper,
        folders: &roots,
        foreign: false,
        prefix: &prefix,
    };
    Err(anyhow!("{}", konedrivectl::explain_sync_error_in(action.about(&named), &error, context)))
}

/// A `Files1` call, for [`explained_paths`].
#[derive(Clone, Copy)]
enum PathAction {
    Hydrate,
    Dehydrate,
    Pin,
    Unpin,
    Free,
}

impl PathAction {
    /// The call, about `named`: the path its refusal is about, or the paths given.
    fn about(self, named: &str) -> SyncAction<'_> {
        match self {
            PathAction::Hydrate => SyncAction::Hydrate(named),
            PathAction::Dehydrate => SyncAction::Dehydrate(named),
            PathAction::Pin => SyncAction::Pin(named),
            PathAction::Unpin => SyncAction::Unpin(named),
            PathAction::Free => SyncAction::Free(named),
        }
    }
}

/// Whether the folder at `path` carries a drive (`user.konedrive.drive`, design §8.3): a
/// registration of it refused `NotEmpty` was refused because it is another account's
/// folder. An empty value is no drive, as the daemon reads it. The link itself, if it is
/// one: the daemon refuses links anyway.
fn carries_a_drive(path: &str) -> bool {
    let (Ok(path), Ok(name)) = (std::ffi::CString::new(path), std::ffi::CString::new("user.konedrive.drive")) else {
        return false;
    };
    // SAFETY: both are NUL-terminated strings that live across the call; a null buffer of
    // size 0 asks only for the value's size.
    unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) > 0 }
}

/// `Some(detail)` when the D-Bus call just made left (or found) the root in
/// `RootState = error` — `LastError` is the detail. Every mutating command
/// re-checks this after its own call succeeds: any of them can run while the
/// root is already unhealthy (a helper that dropped, a recovery that could
/// not finish), and that must not be left out of the command's own exit
/// status just because the specific thing it asked for went through.
async fn root_trouble(proxy: &Sync1Proxy<'_>) -> anyhow::Result<Option<String>> {
    if proxy.root_state().await? == "error" {
        Ok(Some(proxy.last_error().await?))
    } else {
        Ok(None)
    }
}

/// Bails with the detail when [`root_trouble`] finds one. Used after the
/// commands whose own success message is still true regardless (a file
/// really was hydrated, a folder really was forgotten) but where the root as
/// a whole may still need attention.
async fn fail_if_root_unhealthy(proxy: &Sync1Proxy<'_>) -> anyhow::Result<()> {
    if let Some(detail) = root_trouble(proxy).await? {
        bail!("the sync root needs attention: {detail}");
    }
    Ok(())
}

/// [`fail_if_root_unhealthy`] for every folder that holds one of `paths`.
async fn fail_if_holders_unhealthy(daemon: &Daemon, paths: &[String]) -> anyhow::Result<()> {
    let (folders, _) = daemon.folders().await?;
    let mut checked: Vec<&str> = Vec::new();
    for folder in paths.iter().filter_map(|path| holder(&folders, path)) {
        if checked.contains(&folder.root.as_str()) {
            continue;
        }
        checked.push(&folder.root);
        if let Some(detail) = root_trouble(&folder.sync).await? {
            bail!("the sync folder {} needs attention: {detail}", folder.root);
        }
    }
    Ok(())
}

/// The daemon only accepts absolute paths; resolve here so relative ones work.
fn absolute_str(path: &str) -> anyhow::Result<String> {
    // the directory the name is in is resolved, the
    // name itself is not. Canonicalising the whole path resolved a symbolic
    // link given as the last component, so `sync register <link>` silently
    // registered the link's target, while refuses a link as a
    // root; the daemon opens every path it is handed with `O_NOFOLLOW`, and
    // it has to be handed the link to refuse it.
    let given = std::path::Path::new(path);
    std::fs::symlink_metadata(given).with_context(|| format!("no such path: {path}"))?;
    let absolute = match (given.parent(), given.file_name()) {
        (Some(parent), Some(name)) => {
            let parent = if parent.as_os_str().is_empty() { std::path::Path::new(".") } else { parent };
            std::fs::canonicalize(parent)
                .with_context(|| format!("no such path: {path}"))?
                .join(name)
        }
        // `/`, `.`, `..` and the like: no last name to keep.
        _ => std::fs::canonicalize(given).with_context(|| format!("no such path: {path}"))?,
    };
    absolute.to_str().map(str::to_owned).context("non-UTF-8 path")
}

/// `login` (design §5.2): the chosen account's sign-in. With no account at all and none
/// named, it first adds one called `Personal`, so the documented setup — `set-client-id`,
/// `login`, `sync register ~/OneDrive` — keeps working word for word.
async fn login(daemon: &Daemon, option: Option<&str>) -> anyhow::Result<()> {
    let accounts = daemon.accounts().await?;
    // An unreadable configuration first: it also leaves the client ID unknown.
    daemon.check_loaded(&accounts).await?;
    // A name that fits no account, or several accounts and none named, before the client ID.
    let named = match (accounts.is_empty(), wanted(option)) {
        (true, None) => None,
        (_, wanted) => Some(konedrivectl::choose(&accounts, wanted)?.clone()),
    };
    // Before anything is added: every account signs in with it.
    if daemon.manager.client_id().await?.is_empty() {
        bail!(
            "no client ID is set yet. Save the Application (client) ID of your Microsoft Entra app \
             registration first: `konedrivectl set-client-id <id>` (see the README)"
        );
    }
    let chosen = match named {
        Some(chosen) => chosen,
        None => {
            let result = daemon.manager.add(FIRST_LABEL).await;
            let action = AccountAction::Add(FIRST_LABEL);
            let path = result.map_err(|e| anyhow!(konedrivectl::explain_account_error(action, &e)))?;
            let id = daemon.account(&path).await?.id().await?;
            println!("Added an account called {FIRST_LABEL} (`konedrivectl account rename {FIRST_LABEL} <label>` renames it).");
            AccountInfo { path, id, label: FIRST_LABEL.to_owned(), email: String::new() }
        }
    };
    let proxy = daemon.account(&chosen.path).await?;
    let result = proxy.begin_sign_in().await;
    let url = result.map_err(|e| anyhow!(konedrivectl::explain_account_error(AccountAction::SignIn(&chosen.label), &e)))?;
    println!(
        "Opening the Microsoft sign-in page for {} in your browser. If it does not open, visit:\n\n  {url}\n",
        chosen.label
    );
    let _ = Command::new("xdg-open")
        .arg(&url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    // An uncached proxy: the wait loop polls `State` directly rather than watching
    // `StateChanged`, so it never misses a transition the signal stream coalesced away.
    let wait_proxy = Account1Proxy::builder(&daemon.connection)
        .path(chosen.path.clone())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;

    tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(6 * 60), konedrivectl::wait_for_sign_in(&wait_proxy)) => {
            result.context("timed out")??;
        }
        _ = tokio::signal::ctrl_c() => {
            proxy.cancel_sign_in().await?;
            bail!("cancelled");
        }
    }
    match wait_proxy.email().await.unwrap_or_default() {
        email if email.is_empty() => println!("Signed in to {}.", chosen.label),
        email => println!("Signed in to {} as {email}.", chosen.label),
    }
    Ok(())
}
