//! `Accounts.SignIn`: a sign-in for a new account. It belongs to no account: the manager
//! runs it, and until the browser sign-in has succeeded and its drive is known not to be
//! another account's, nothing exists for it — no id, no entry in `config.toml`, no
//! directory, no stored token, no object on the bus. Then the account is made, already
//! signed in. One at a time; each has a number, and ends once.

use std::sync::Arc;

use konedrive_dbus::account_path;
use konedrive_dbus::sign_in::{ALREADY_ADDED, CANCELLED, FAILED, SIGNED_IN};
use tokio::sync::oneshot;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::Connection;

use super::{follow_mode, remove_account_dir, Account, AccountManager, ManagerError};
use crate::account::attempt::{self, Attempt, SignedIn};
use crate::account::{claim_new, AccountError, Refused};
use crate::config::Mode;

/// The sign-ins `Accounts.SignIn` answered: the number of the last one, which is never
/// given again while the daemon runs, and the one under way.
#[derive(Default)]
pub(super) struct SignIns {
    last: u32,
    under_way: Option<UnderWay>,
}

/// The sign-in that has not ended: whoever takes it out of [`SignIns`] ends it, and says
/// how.
struct UnderWay {
    number: u32,
    /// Ends its wait for the browser.
    cancel: oneshot::Sender<()>,
    /// The connection its end is said on.
    connection: Connection,
}

/// How a sign-in ended: the outcome, the message and the account of `SignInFinished`.
struct End {
    outcome: &'static str,
    message: String,
    account: Option<OwnedObjectPath>,
}

impl End {
    fn failed(why: impl Into<String>) -> Self {
        Self { outcome: FAILED, message: why.into(), account: None }
    }
}

impl AccountManager {
    /// `Accounts.SignIn`: starts a sign-in for a new account; its number and the URL to
    /// open. One under way is ended as `cancelled` first. Refused, with nothing started,
    /// when `config.toml` could not be loaded, no client ID can be had or the listener
    /// cannot be bound.
    ///
    /// The sign-in ends once: when its browser sign-in ends
    /// ([`finish_sign_in`](Self::finish_sign_in)), at a [`cancel_sign_in`](Self::cancel_sign_in),
    /// or at the next `SignIn` or `SetClientId`.
    pub async fn sign_in(self: &Arc<Self>, connection: &Connection) -> Result<(u32, String), ManagerError> {
        let _changing = self.changing.lock().await;
        // The account could not be written at the end: said before the browser is opened.
        if self.config.is_poisoned() {
            return Err(ManagerError::Failed(self.config.last_error()));
        }
        self.end_sign_in(None).await;
        let client_id = self.config.client_id();
        if client_id.is_empty() {
            return Err(AccountError::NoClientId.into());
        }
        let http = attempt::http_client().map_err(|e| ManagerError::Failed(e.to_string()))?;
        let graph = attempt::graph(&self.options.endpoints).map_err(|e| ManagerError::Failed(e.to_string()))?;
        // Read-only, with Microsoft's account picker: a new account is any account.
        let oauth = attempt::oauth_client(http, self.options.endpoints.clone(), &client_id, Mode::ReadOnly);
        let browser = Attempt::bind(oauth).await.map_err(ManagerError::Failed)?;
        let (url, asked) = (browser.picker_url(), browser.scope());
        let (cancel, cancelled) = oneshot::channel();
        let number = {
            let mut sign_ins = crate::panic::lock(&self.sign_ins);
            sign_ins.last = sign_ins.last.wrapping_add(1);
            sign_ins.under_way = Some(UnderWay { number: sign_ins.last, cancel, connection: connection.clone() });
            sign_ins.last
        };
        // Weak: the task waits for the browser, and holds nothing of the daemon meanwhile.
        let (manager, timeout) = (Arc::downgrade(self), self.options.sign_in_timeout);
        tokio::spawn(async move {
            let signed_in = match browser.tokens(timeout, cancelled).await {
                Ok(tokens) => attempt::confirm(tokens, &graph).await.map_err(|unconfirmed| unconfirmed.sentence()),
                Err(why) => Err(why),
            };
            let Some(manager) = manager.upgrade() else { return };
            manager.finish_sign_in(number, asked, signed_in).await;
        });
        tracing::info!("signing a new account in (sign-in {number})");
        Ok((number, url))
    }

    /// `Accounts.CancelSignIn`: ends the sign-in `number` as `cancelled`. A number that is
    /// not under way — ended already, or being made an account — is ignored. It waits for
    /// nothing: not for a change of the accounts, nor for the wallet.
    pub async fn cancel_sign_in(&self, number: u32) {
        self.end_sign_in(Some(number)).await;
    }

    /// Takes the sign-in under way, `only` the one with that number: whoever gets it ends
    /// it, so it ends once.
    fn take_sign_in(&self, only: Option<u32>) -> Option<UnderWay> {
        let mut sign_ins = crate::panic::lock(&self.sign_ins);
        match &sign_ins.under_way {
            Some(under_way) if only.is_none_or(|number| under_way.number == number) => sign_ins.under_way.take(),
            _ => None,
        }
    }

    /// Ends the sign-in under way, `only` the one with that number, as `cancelled`: its
    /// wait for the browser ends, and whatever the browser still answers is dropped.
    pub(super) async fn end_sign_in(&self, only: Option<u32>) {
        let Some(UnderWay { number, cancel, connection }) = self.take_sign_in(only) else { return };
        let _ = cancel.send(());
        tracing::info!("the sign-in {number} of a new account is cancelled");
        self.say_finished(&connection, number, End { outcome: CANCELLED, message: String::new(), account: None }).await;
    }

    /// The browser sign-in of `number` ended: `signed_in`, or why there is none. Nothing
    /// follows when the sign-in was ended meanwhile. Otherwise it is under way no longer,
    /// the account is made ([`make_signed_in`](Self::make_signed_in)) with `changing` held,
    /// and the outcome said.
    async fn finish_sign_in(&self, number: u32, asked: &'static str, signed_in: Result<SignedIn, String>) {
        let _changing = self.changing.lock().await;
        let Some(UnderWay { connection, .. }) = self.take_sign_in(Some(number)) else { return };
        let end = match signed_in {
            // Not reached: a wait that was cancelled is under way no longer.
            Err(why) if why.is_empty() => End { outcome: CANCELLED, message: why, account: None },
            Err(why) => End::failed(why),
            Ok(signed_in) => match self.make_signed_in(&connection, &signed_in, asked).await {
                Ok(account) => {
                    let label = account.account.state().get().label;
                    tracing::info!("added the account {label:?} ({})", account.id);
                    End { outcome: SIGNED_IN, message: label, account: Some(account.path.clone()) }
                }
                Err(Refused::AlreadyConnected { id, label }) => End { outcome: ALREADY_ADDED, message: label, account: account_path(&id) },
                Err(Refused::Other(why)) => End::failed(why),
            },
        };
        if end.outcome != SIGNED_IN {
            tracing::info!("the sign-in {number} of a new account ended: {} {}", end.outcome, end.message);
        }
        self.say_finished(&connection, number, end).await;
    }

    /// The account of a sign-in that succeeded, made already signed in. Called with
    /// `changing` held.
    ///
    /// 1. The identity guard, as an account's own sign-in asks it: the other accounts'
    ///    drives settled, then — in one write of `config.toml` — the check, and the entry
    ///    it allows, with its label, drive and `login_hint`. Refused, nothing was made.
    /// 2. The account built, its token stored and its session signed in.
    /// 3. Listed, and put on the bus.
    ///
    /// A step that fails takes back what the steps before it made. The entry is written
    /// before the token is stored, so that a daemon that stops in between leaves a
    /// signed-out account anybody can see, sign in again or remove — never a token in the
    /// wallet that no account owns.
    async fn make_signed_in(&self, connection: &Connection, signed_in: &SignedIn, asked: &'static str) -> Result<Arc<Account>, Refused> {
        let identity = &signed_in.identity;
        let unsettled = self.siblings.settle().await;
        let entry = self.config.update(|config| claim_new(config, &identity.drive, identity.email.as_deref(), &unsettled))?;
        let account = match self.build(&entry) {
            Ok(account) => account,
            Err(e) => {
                // What was made before it failed: the entry, the directory, and its place
                // among the accounts the identity guard looks at.
                if let Err(e) = self.config.remove_account(&entry.id) {
                    tracing::warn!("cannot take the account {:?} out of config.toml again: {e}", entry.label);
                }
                if let Some(paths) = self.paths.account(&entry.id) {
                    remove_account_dir(&paths.dir);
                }
                self.siblings.remove(&entry.id);
                return Err(Refused::Other(e.to_string()));
            }
        };
        if let Err(e) = account.account.start_signed_in(signed_in, asked).await {
            self.unmake(&account, connection, false).await;
            return Err(Refused::Other(e));
        }
        follow_mode(&account).await;
        // Listed before it is on the bus, as an account `add` makes is.
        self.list(Arc::clone(&account));
        if let Err(e) = self.export(connection, &account).await {
            self.unmake(&account, connection, true).await;
            return Err(Refused::Other(format!("cannot put the account on the bus: {e}")));
        }
        // The name and the quota come after the account does.
        let service = Arc::clone(&account.account);
        tokio::spawn(async move { service.refresh_account_info().await });
        Ok(account)
    }

    /// Leaves nothing of an account [`make_signed_in`](Self::make_signed_in) could not
    /// finish, as `Accounts.Remove` leaves nothing of an account with no folder: no object
    /// on the bus, no stored token, no entry in `config.toml`, no directory. `listed`:
    /// it was listed already.
    async fn unmake(&self, account: &Account, connection: &Connection, listed: bool) {
        self.unexport(connection, account, true).await;
        if listed {
            self.unlist(account);
        }
        if let Err(e) = account.account.retire().await {
            tracing::warn!("cannot delete the sign-in of the account {}, which could not be added: {e}", account.id);
        }
        if let Err(e) = self.config.remove_account(&account.id) {
            tracing::warn!("cannot take the account {} out of config.toml again: {e}", account.id);
        }
        remove_account_dir(&account.paths.dir);
        self.siblings.remove(&account.id);
    }

    /// `Accounts.SignInFinished` for the sign-in `number`, after the change of
    /// `Accounts.List` when it made an account.
    async fn say_finished(&self, connection: &Connection, number: u32, end: End) {
        let none = ObjectPath::from_static_str_unchecked("/");
        let account = end.account.as_ref().map_or(none, |path| path.as_ref());
        let listed = end.outcome == SIGNED_IN;
        if let Err(e) = self.options.bus.sign_in_finished(connection, number, end.outcome, &end.message, &account, listed).await {
            tracing::warn!("cannot say how the sign-in {number} ended: {e}");
        }
    }
}
