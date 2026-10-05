use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "konedrivectl",
    disable_version_flag = true,
    arg_required_else_help = true,
    about = "Control the KOneDrive daemon",
    after_help = "Choosing the account: a command that acts on one account uses the one --account names, \
                  else the one KONEDRIVE_ACCOUNT names, else the only account there is. With several \
                  accounts and none named, or a name that fits more than one, it stops and lists them. \
                  `status` and `sync status` show every account when none is named. The commands that take \
                  a path (`sync hydrate`, `dehydrate`, `state`, `pin`, `unpin`, `free`, `open`) act on the \
                  account whose folder holds the path; they, `account list`, `account add`, `account \
                  rename`, `account remove`, `set-client-id`, `settings` and `sync anyway --all` refuse \
                  --account and ignore KONEDRIVE_ACCOUNT."
)]
pub(crate) struct Cli {
    /// The account to act on: its id, its label or its email, as `account list` shows them
    /// (label and email in any case). Without it, KONEDRIVE_ACCOUNT; without that, the only
    /// account there is
    #[arg(long, global = true, value_name = "ACCOUNT", allow_hyphen_values = true)]
    pub(crate) account: Option<String>,
    /// Print this program's version and commit, then the running daemon's; says so when the
    /// daemon runs another build and should be restarted
    #[arg(short = 'V', long)]
    pub(crate) version: bool,
    #[command(subcommand)]
    pub(crate) command: Option<Cmd>,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
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
    /// Show or change the settings every account shares: what they do on a metered
    /// connection and on battery
    Settings {
        #[command(subcommand)]
        command: SettingsCmd,
    },
    /// Sign an account that is signed out in again with its Microsoft account, in the browser
    ///
    /// It adds no account: `account add` does. The sign-in page's address is
    /// printed; it is also opened in the browser with xdg-open, but only when stdout is a
    /// terminal and KONEDRIVE_NO_BROWSER is not set (set it for a sign-in over SSH).
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
    /// Development tools: only in a development build (the `dev-tools` feature)
    #[cfg(feature = "dev-tools")]
    Dev {
        #[command(subcommand)]
        command: DevCmd,
    },
}

#[derive(Subcommand)]
pub(crate) enum AccountCmd {
    /// List every account: id, label, email, sign-in state, mode, and folder with its state
    List,
    /// Add an account by signing in to OneDrive in the browser; it is named by its email
    ///
    /// The sign-in page's address is printed; it is also opened in the browser with xdg-open,
    /// but only when stdout is a terminal and KONEDRIVE_NO_BROWSER is not set (set it for a
    /// sign-in over SSH). Nothing is added unless the sign-in succeeds, and a OneDrive
    /// account that is added already is not added again. `account rename` gives the account
    /// another label.
    Add,
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
pub(crate) enum SettingsCmd {
    /// Show or change what every account does on a metered connection: pause, or sync as
    /// usual
    OnMetered {
        #[arg(value_parser = ["pause", "sync"])]
        choice: Option<String>,
    },
    /// Show or change what every account does on battery: sync as usual, pause in
    /// power-saver mode, or pause
    OnBattery {
        #[arg(value_parser = ["sync", "power-saver", "pause"])]
        choice: Option<String>,
    },
}

#[cfg(feature = "dev-tools")]
#[derive(Subcommand)]
pub(crate) enum DevCmd {
    /// Add an account under a label, signed out and with no folder yet, which never has to
    /// sign in: for a folder that shows a local directory (`sync
    /// register-without-interception`, `sync populate-from`)
    // The rule is the daemon's sentence, so the help says what the daemon takes.
    #[command(long_about = format!(
        "Add an account under a label, signed out and with no folder yet, which never has to sign in: for a \
         folder that shows a local directory\n\n{}.",
        konedrive_dbus::LABEL_RULE
    ))]
    AddAccount {
        #[arg(allow_hyphen_values = true)]
        label: String,
    },
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

/// `sync`: `status`, the commands on the chosen account's folder, and the commands that take
/// a path, which decides the account.
#[derive(Subcommand)]
pub(crate) enum SyncCmd {
    /// Show the account's folder and its state; every account's when there are several and
    /// none is chosen
    Status,
    #[command(flatten)]
    Folder(FolderCmd),
    #[command(flatten)]
    Path(PathCmd),
}

/// The `sync` commands that act on the chosen account's folder.
#[derive(Subcommand)]
pub(crate) enum FolderCmd {
    /// Bind an empty folder to the account
    Register { path: String },
    /// The developer's mode: bind a local folder with NOTHING intercepting opens inside it
    ///
    /// The folder is filled from a directory with `populate-from`. Without
    /// the helper, files that are not downloaded read as zeros until you
    /// `hydrate` them by hand. It never shows OneDrive: that takes `register`
    /// and the helper. Named after the D-Bus method it calls
    /// (`Folder.RegisterWithoutInterception`) rather than something shorter, on
    /// purpose: the cost this mode carries belongs in the word you type, not
    /// just in a warning you might scroll past.
    RegisterWithoutInterception { path: String },
    /// Forget the account's folder (local files are left as they are)
    Forget,
    /// Fill the account's folder with placeholders mirroring a local directory
    PopulateFrom { source_dir: String },
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
    /// Sync now though the account paused by itself (a metered connection, the battery),
    /// until the connection, the battery or the power profile changes
    Anyway {
        /// Every account that paused by itself and is not paused by you, as the tray's
        /// Sync Anyway does
        #[arg(long)]
        all: bool,
    },
    /// Show or change whether OneDrive's thumbnails of images and videos are downloaded.
    /// Off, Dolphin downloads a cloud-only file in full to show its preview while its
    /// previews are on
    Thumbnails {
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
    },
    /// Show or change the names of local files that are never uploaded (shell
    /// globs, matched against a name)
    Ignore {
        #[command(subcommand)]
        action: Option<IgnoreCmd>,
    },
    /// List what stays on this computer, and why: every reason with its
    /// count, then the files of the reasons that need something done to each
    NotUploaded {
        /// List every file of every reason, not only the first 20 of each
        /// reason that needs something done to each file
        #[arg(long)]
        all: bool,
    },
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
}

/// The `sync` commands that take a path: `Files` finds the account by it.
#[derive(Subcommand)]
pub(crate) enum PathCmd {
    /// Download one file now. The path decides the account
    Hydrate { path: String },
    /// Free up space for one file. The path decides the account
    Dehydrate { path: String },
    /// Print one file's state. The path decides the account
    State { path: String },
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
    /// Open the page of a file or folder in OneDrive's web interface, where it can be
    /// shared and its versions seen; the account's folder itself opens the drive. The
    /// address is printed either way. The path decides the account
    Open {
        path: String,
        /// Only print the address; open nothing
        #[arg(long)]
        print: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum IgnoreCmd {
    /// Print the list
    List,
    /// Add a pattern, such as `*.bak`
    Add { pattern: String },
    /// Remove a pattern
    Remove { pattern: String },
}

#[derive(Subcommand)]
pub(crate) enum DeletesCmd {
    /// Delete them in OneDrive too
    Confirm,
    /// Keep them in OneDrive: they come back here
    Restore,
}
