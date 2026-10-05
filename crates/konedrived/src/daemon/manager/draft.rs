//! The draft of `Accounts.SignIn`: a new account that is on the bus and in `config.toml`,
//! marked there, but in none of the manager's lists until its sign-in succeeds. One at a
//! time; it ends once, and every end but a sign-in that succeeded leaves nothing of it.

use std::sync::Arc;
use std::time::Duration;

use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

use super::{follow_mode, remove_account_dir, Account, AccountManager, ManagerError};
use crate::account::secret::Slot;
use crate::account::SignInEnd;
use crate::config::AccountId;

/// The one draft there is, and the connection its objects are on.
pub(super) struct Draft {
    pub(super) account: Arc<Account>,
    pub(super) connection: Connection,
}

/// How long the start waits for the wallet to delete what a draft left in it.
const WALLET_DELETE: Duration = Duration::from_secs(10);

impl AccountManager {
    /// The start's first step with the accounts, before any comes up: every entry of
    /// `config.toml` still marked as a draft — a sign-in the daemon did not see the end
    /// of — is removed, with its stored token and its directory. Nobody is told.
    pub async fn remove_drafts(&self) {
        for entry in self.config.snapshot().accounts.iter().filter(|a| a.draft) {
            tracing::info!("removing the unfinished sign-in {} left in config.toml", entry.id);
            let slot = Slot::Account(entry.id.clone());
            match tokio::time::timeout(WALLET_DELETE, self.options.wallet.delete(&slot)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("cannot delete the token of the unfinished sign-in {}: {e}", entry.id),
                Err(_) => tracing::warn!("the wallet did not answer; the token of the unfinished sign-in {} may be left", entry.id),
            }
            if let Some(paths) = self.paths.account(&entry.id) {
                remove_account_dir(&paths.dir);
            }
            if let Err(e) = self.config.remove_account(&entry.id) {
                tracing::warn!("cannot take the unfinished sign-in {} out of config.toml: {e}", entry.id);
            }
        }
    }

    /// `Accounts.SignIn`: a new account as a *draft* — in `config.toml` marked as one, on
    /// the bus, in no list, taking no folder — with its sign-in begun; its path and the URL
    /// to open. A draft there already is ended first, as a cancel ends it. What
    /// `BeginSignIn` refuses is refused under its name, and nothing is left then.
    ///
    /// The draft ends once ([`end_draft`](Self::end_draft)): when its account says how the
    /// sign-in ended, at the next `SignIn` or `SetClientId`, or at an `Accounts.Remove` of
    /// its path.
    pub async fn sign_in(self: &Arc<Self>, connection: &Connection) -> Result<(OwnedObjectPath, String), ManagerError> {
        let _changing = self.changing.lock().await;
        self.end_draft(None, SignInEnd::Cancelled).await;
        let entry = self.config.add_draft()?;
        let account = match self.build(&entry) {
            Ok(account) => account,
            Err(e) => {
                if let Err(e) = self.config.remove_account(&entry.id) {
                    tracing::warn!("cannot take the draft {} out of config.toml again: {e}", entry.id);
                }
                // What was built before it failed: the directory, and its place among the
                // accounts the identity guard looks at.
                if let Some(paths) = self.paths.account(&entry.id) {
                    remove_account_dir(&paths.dir);
                }
                self.siblings.remove(&entry.id);
                return Err(ManagerError::Failed(e.to_string()));
            }
        };
        follow_mode(&account).await;
        let (ends, ended) = tokio::sync::mpsc::unbounded_channel();
        account.account.tell_sign_in_end(Some(ends));
        // A draft takes no folder: retired as an account being removed is, until it is an
        // account.
        let begun = match account.sync.retire().await {
            Ok(_) => match self.export(connection, &account).await {
                Ok(()) => account.account.begin_sign_in().await.map_err(ManagerError::from),
                Err(e) => Err(ManagerError::Failed(format!("cannot put the account on the bus: {e}"))),
            },
            Err(e) => Err(e.into()),
        };
        let url = match begun {
            Ok(url) => url,
            Err(e) => {
                self.discard(&account, connection, true).await;
                return Err(e);
            }
        };
        *crate::panic::lock(&self.draft) = Some(Draft { account: Arc::clone(&account), connection: connection.clone() });
        // Weak: the task waits as long as the draft does, and the draft is the manager's.
        let (manager, id) = (Arc::downgrade(self), account.id.clone());
        tokio::spawn(async move {
            let mut ended = ended;
            let Some(end) = ended.recv().await else { return };
            let Some(manager) = manager.upgrade() else { return };
            let _changing = manager.changing.lock().await;
            manager.end_draft(Some(&id), end).await;
        });
        tracing::info!("signing a new account in ({})", account.id);
        Ok((account.path.clone(), url))
    }

    /// Ends the draft, once: `only` the draft with that id, so that an end said late — for
    /// a draft a newer `SignIn` ended already — ends nothing. Called with `changing` held.
    ///
    /// A sign-in that succeeded makes it an account: the label and the draft mark in one
    /// write of `config.toml`, then listed, then `SignInFinished`. Every other end leaves
    /// nothing of it.
    pub(super) async fn end_draft(&self, only: Option<&AccountId>, end: SignInEnd) {
        let Draft { account, connection } = {
            let mut draft = crate::panic::lock(&self.draft);
            match draft.as_ref() {
                Some(current) if only.is_none_or(|id| current.account.id == *id) => {}
                _ => return,
            }
            let Some(draft) = draft.take() else { return };
            draft
        };
        use konedrive_dbus::sign_in::{ALREADY_ADDED, CANCELLED, FAILED, SIGNED_IN};
        let (outcome, message) = match end {
            SignInEnd::SignedIn { email } => match self.config.finish_draft(&account.id, email.as_deref()) {
                Ok(label) => (SIGNED_IN, label),
                Err(e) => (FAILED, format!("the account could not be saved: {e}")),
            },
            SignInEnd::Cancelled => (CANCELLED, String::new()),
            SignInEnd::AlreadyAdded(label) => (ALREADY_ADDED, label),
            SignInEnd::Failed(why) => (FAILED, why),
        };
        let listed = outcome == SIGNED_IN;
        if listed {
            account.account.tell_sign_in_end(None);
            account.account.show_label(&message);
            account.sync.unretire().await;
            self.list(Arc::clone(&account));
            tracing::info!("added the account {message:?} ({})", account.id);
        } else {
            tracing::info!("the sign-in of a new account ({}) ended: {outcome} {message}", account.id);
            self.discard(&account, &connection, false).await;
        }
        let path = account.path.as_ref();
        if let Err(e) = self.options.bus.sign_in_finished(&connection, &path, outcome, &message, listed).await {
            tracing::warn!("cannot say how the sign-in at {path} ended: {e}");
        }
    }

    /// Leaves nothing of an account that never was in the lists, as `Accounts.Remove`
    /// leaves nothing of a signed-out account with no folder: no sign-in, no stored token,
    /// no entry in `config.toml`, no directory, no object on the bus.
    async fn discard(&self, account: &Account, connection: &Connection, partly: bool) {
        account.account.tell_sign_in_end(None);
        if let Err(e) = account.account.retire().await {
            tracing::warn!("cannot delete the sign-in of the draft {}: {e}", account.id);
        }
        if let Err(e) = self.config.remove_account(&account.id) {
            tracing::warn!("cannot take the draft {} out of config.toml: {e}", account.id);
        }
        remove_account_dir(&account.paths.dir);
        self.unexport(connection, account, partly).await;
        self.siblings.remove(&account.id);
    }
}
