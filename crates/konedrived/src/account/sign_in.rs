use super::*;

/// Why the identity guard ([`AccountService::claim`], [`claim_new`]) refuses a sign-in.
#[derive(Debug)]
pub(crate) enum Refused {
    /// Another account has the drive: that account.
    AlreadyConnected { id: AccountId, label: String },
    /// Any other refusal, as it is said.
    Other(String),
}

impl From<ConfigError> for Refused {
    fn from(error: ConfigError) -> Self {
        Refused::Other(error.to_string())
    }
}

impl Refused {
    /// The sentence `LastError` shows.
    fn sentence(&self) -> String {
        match self {
            Refused::AlreadyConnected { label, .. } => format!("This Microsoft account is already connected as '{label}'."),
            Refused::Other(sentence) => sentence.clone(),
        }
    }
}

/// What the identity guard refuses whichever account signs in (§8.2): a drive that an
/// account other than `me` has, and a drive that one of `unsettled` — the others that may
/// be this drive without saying so ([`settle`]) — cannot be told apart from.
fn drive_is_free(config: &Config, me: Option<&AccountId>, drive: &DriveId, unsettled: &[AccountId]) -> Result<(), Refused> {
    let others = || config.accounts.iter().filter(|a| Some(&a.id) != me);
    if let Some(other) = others().find(|a| a.drive_id.as_ref() == Some(drive)) {
        return Err(Refused::AlreadyConnected { id: other.id.clone(), label: other.label.clone() });
    }
    if others().any(|a| a.drive_id.is_none() && unsettled.contains(&a.id)) {
        return Err(Refused::Other("Could not check which account this is; try again.".into()));
    }
    Ok(())
}

/// The identity guard for a sign-in that belongs to no account yet (`Accounts.SignIn`),
/// and the account it allows, in one change of `config` — so one `ConfigStore` update, as
/// [`AccountService::claim`] is: when no other account has `drive`, a new account after
/// every other, under a fresh id, with the drive, the `email` as its `login_hint`, and
/// its label ([`crate::config::signed_in_label`]). `Err` says why the sign-in is refused, and nothing was
/// added.
pub(crate) fn claim_new(config: &mut Config, drive: &DriveId, email: Option<&str>, unsettled: &[AccountId]) -> Result<AccountConfig, Refused> {
    drive_is_free(config, None, drive, unsettled)?;
    let id = AccountId::fresh(config.accounts.iter().map(|a| &a.id));
    let label = crate::config::signed_in_label(config, &id, email);
    let mut account = AccountConfig::new(id, label, Origin::Added);
    account.drive_id = Some(drive.clone());
    if let Some(email) = email.filter(|email| !email.is_empty()) {
        account.login_hint = email.to_owned();
    }
    config.accounts.push(account.clone());
    Ok(account)
}

/// Of `accounts`, those the guard cannot tell apart from a drive (§8.2), by id: those with
/// no drive recorded that may be signed in to one — signed in, or holding a refresh token
/// they have not used yet (a wallet that did not answer at startup leaves an account signed
/// out with its token still stored). Each is asked for its drive first, and is left out
/// once it has one.
pub(super) async fn settle(accounts: Vec<Arc<AccountService>>) -> Vec<AccountId> {
    let mut unsettled = Vec::new();
    for other in accounts {
        if other.recorded_drive().is_some() {
            continue;
        }
        let signed_in = other.state.get().state == SignInState::SignedIn;
        if !signed_in && matches!(other.secrets.exists().await, Ok(false)) {
            continue;
        }
        if let Err(e) = other.learn_drive().await {
            tracing::warn!("cannot learn the drive of account {:?}: {e}", other.id);
        }
        if other.recorded_drive().is_none() {
            unsettled.push(other.id.clone());
        }
    }
    unsettled
}

impl AccountService {
    /// Starts a sign-in and returns the URL the user must open in a browser.
    ///
    /// The start is one step under the session lock: the state becomes `signing-in`, the
    /// generation is bumped and the cancel channel becomes this attempt's, with nothing of a
    /// cancel or a sign-out in between. So an attempt is live only while the account shows
    /// `signing-in`, and whatever ends that state ends the attempt. A start that is refused
    /// leaves the state as it was.
    ///
    /// What can be refused without the lock is refused before it is taken: the lock is held
    /// across a wallet prompt (a commit) and across a refresh in flight (a sign-out), and an
    /// account that is signed in or signing in answers `Busy` at once, not after them. The
    /// same is asked again under the lock, which is what decides.
    pub async fn begin_sign_in(self: &Arc<Self>) -> Result<String, AccountError> {
        self.may_begin_sign_in()?;
        if self.state.get().state != SignInState::SignedOut {
            return Err(AccountError::Busy);
        }
        let mut session = self.session.lock().await;
        // Asked again under the lock: a removal's sign-out, a sign-in or a restored session
        // that came first leaves no attempt behind.
        let client_id = self.may_begin_sign_in()?;
        if !self.state.try_transition(SignInState::SignedOut, SignInState::SigningIn) {
            return Err(AccountError::Busy);
        }
        self.state.update(|s| s.clear_error());
        // A read-write account signing in again asks for Files.ReadWrite again (§7).
        let browser = match Attempt::bind(self.oauth_client(&client_id, self.sign_in_mode())).await {
            Ok(browser) => browser,
            Err(message) => {
                self.state.update(|s| {
                    s.state = SignInState::SignedOut;
                    s.set_error(message.clone());
                });
                return Err(AccountError::Failed(message));
            }
        };
        let url = self.authorize_url(&browser);
        let (cancel_tx, cancel_rx) = oneshot::channel();
        session.generation += 1;
        session.cancel = Some(cancel_tx);
        let generation = session.generation;
        drop(session);
        let attempt = SignInAttempt { browser, cancel: cancel_rx, generation };
        let this = Arc::clone(self);
        tokio::spawn(async move { this.finish_sign_in(attempt).await });
        Ok(url)
    }

    /// What refuses a sign-in whatever the state is, in the order it is said; otherwise the
    /// client id the sign-in uses. Changes nothing.
    fn may_begin_sign_in(&self) -> Result<String, AccountError> {
        if self.is_retired() {
            return Err(AccountError::Failed(RETIRED.into()));
        }
        let client_id = self.state.get().client_id;
        if client_id.is_empty() {
            return Err(AccountError::NoClientId);
        }
        Ok(client_id)
    }

    /// The authorization URL of the attempt `browser`: Microsoft's account picker for a
    /// read-only sign-in, so that any account can be chosen; pinned to this account whenever it
    /// asks for `Files.ReadWrite` — its password asked for again, with its email
    /// filled in when it is known: from the state, `account.json`, or — after a sign-out,
    /// which forgets both — the `login_hint` `config.toml` keeps.
    pub(super) fn authorize_url(&self, browser: &Attempt) -> String {
        if !grants_writes(browser.scope()) {
            return browser.picker_url();
        }
        let email = [
            Some(self.state.get().email),
            crate::account::cache::load(&self.cache).map(|info| info.email),
            self.config.account(&self.id).map(|account| account.login_hint),
        ]
        .into_iter()
        .flatten()
        .find(|email| !email.is_empty());
        browser.oauth.pinned_authorize_url(&browser.redirect_uri, &browser.pkce, &browser.csrf, email.as_deref()).to_string()
    }

    /// Stops waiting for the browser and returns to `signed-out` with no `LastError`,
    /// whether the browser has answered yet or not. If a callback already arrived and is
    /// being exchanged, that exchange is left to finish but its result is discarded: this
    /// bumps the generation, so the exchange's eventual commit sees itself superseded.
    pub async fn cancel_sign_in(&self) {
        let mut session = self.session.lock().await;
        session.generation += 1;
        if let Some(cancel) = session.cancel.take() {
            let _ = cancel.send(());
        }
        self.state.update(|s| {
            if s.state == SignInState::SigningIn {
                s.state = SignInState::SignedOut;
                s.clear_error();
            }
        });
    }

    /// `Accounts.Remove`'s sign-out: the account retired first, so that no sign-in is
    /// begun or stored from then on — one under way is superseded by the sign-out, and one
    /// whose exchange ends later finds the account retired — then signed out as
    /// [`sign_out`](Self::sign_out) signs out.
    pub async fn retire(&self) -> Result<(), AccountError> {
        self.retired.store(true, std::sync::atomic::Ordering::SeqCst);
        self.sign_out().await
    }

    /// Takes [`retire`](Self::retire) back, for an `Accounts.Remove` that failed: the
    /// account stays, so it signs in and changes its mode again. What `retire` did stays
    /// done: a sign-in that was under way is given up, and a sign-in that was deleted is
    /// gone.
    pub fn unretire(&self) {
        self.retired.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    pub(super) fn is_retired(&self) -> bool {
        self.retired.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Signs the account out: a sign-in under way is given up, the refresh token deleted,
    /// the cached name and quota forgotten.
    ///
    /// When the wallet refuses the delete, this fails and the token stays. An account that
    /// was signed in stays signed in, as it was. One that was signing in is `signed-out` by
    /// then, its attempt given up: that does not wait for the wallet.
    pub async fn sign_out(&self) -> Result<(), AccountError> {
        // Held for the whole call: supersedes any in-flight sign-in attempt, and makes
        // this mutually exclusive with `refresh_account_info`'s cache-save-and-apply step,
        // so neither a stale sign-in nor a stale refresh can resurrect state afterwards.
        let mut session = self.session.lock().await;
        session.generation += 1;
        if let Some(cancel) = session.cancel.take() {
            let _ = cancel.send(());
        }
        // The attempt is over whatever the wallet answers below: an account never shows
        // `signing-in` with no attempt behind it.
        self.state.update(|s| {
            if s.state == SignInState::SigningIn {
                s.state = SignInState::SignedOut;
                s.clear_error();
            }
        });
        // Takes TokenManager's own lock, so an in-flight access-token refresh commits its
        // rotated refresh token to the wallet before this deletes it.
        self.tokens.forget().await.map_err(|e| AccountError::Failed(e.to_string()))?;
        crate::account::cache::remove(&self.cache);
        self.state.update(|s| {
            s.state = SignInState::SignedOut;
            s.clear_error();
            s.clear_account();
        });
        // Nothing is granted any more: read-only until the next sign-in, which asks for the
        // mode config.toml keeps.
        self.recompute_mode();
        Ok(())
    }

    pub async fn refresh_account_info(&self) {
        if self.state.get().state != SignInState::SignedIn {
            return;
        }
        let Ok(token) = self.token_for_refresh().await else { return };
        let graph = self.graph();
        let mut result = tokio::try_join!(graph.profile(&token), graph.drive(&token));
        if matches!(&result, Err(e) if e.status() == Some(Status::UNAUTHORIZED)) {
            // The cached access token was rejected (e.g. its lifetime elapsed across a
            // suspend, which `Instant`-based expiry cannot see coming). Invalidate it and
            // retry once with a freshly refreshed one before giving up.
            self.tokens.invalidate().await;
            let Ok(token) = self.token_for_refresh().await else { return };
            result = tokio::try_join!(graph.profile(&token), graph.drive(&token));
        }
        let (profile, drive) = match result {
            Ok(both) => both,
            Err(e) => {
                self.state
                    .update(|s| s.set_error(format!("Could not load account info: {e}")));
                return;
            }
        };
        let (display_name, email) = (profile.display_name, profile.email);
        // Same lock `sign_out` holds for its whole call: if a sign-out is in progress (or
        // ran to completion while the Graph calls above were in flight), either this waits
        // until it is done and then sees `SignedOut` below, or it beats a sign-out that is
        // still waiting for this lock.
        let _session = self.session.lock().await;
        if self.state.get().state != SignInState::SignedIn {
            return;
        }
        // The drive comes with the quota (§8.1): an account that has none recorded yet —
        // migrated from a folder that never recorded one — records it now.
        if let Err(e) = self.record_drive(&drive.id) {
            tracing::warn!("cannot record the drive of account {:?}: {e}", self.id);
        }
        self.secrets.describe(&email);
        // The granted scopes as the last refresh left them, and the drive the token was just
        // seen to reach, kept beside the name; the quota is kept by its own read, below.
        let granted_scopes = self.state.get().granted_scopes;
        self.save_cache(|info| {
            info.display_name = display_name.clone();
            info.email = email.clone();
            info.fetched_at = u64::try_from(crate::clock::unix_now()).unwrap_or(0);
            info.granted_scopes = granted_scopes;
            info.drive_id = drive.id.clone();
        });
        let mut signed_in = false;
        self.state.update(|s| {
            if s.state == SignInState::SignedIn {
                signed_in = true;
                s.display_name = display_name.clone();
                s.email = email.clone();
                s.live_drive = drive.id.clone();
                s.clear_error();
            }
        });
        // One quota for the account: this read is the one `QuotaRemaining` shows, and the one
        // the uploads' space check next uses when it may reuse a read.
        if signed_in {
            self.quota.read(&drive.quota);
        }
        // Says again why a read-write account runs read-only, if it does: the drive may have
        // just been recorded, or be another than config.toml's, and the line above cleared the
        // reason. A drive that is not the recorded one turns a read-write account read-only.
        self.recompute_mode();
        // The folder's outbox decides by the quota just read whether OneDrive is still full.
        let uploads = crate::panic::lock(&self.uploads).as_ref().and_then(Weak::upgrade);
        if let Some(uploads) = uploads {
            uploads.quota_read(&drive.quota);
        }
    }

    /// The drive recorded for this account, if one is.
    fn recorded_drive(&self) -> Option<DriveId> {
        self.config.account(&self.id).and_then(|a| a.drive_id)
    }

    /// Records `drive` as this account's when it has none yet; the drive recorded.
    fn record_drive(&self, drive: &str) -> Result<DriveId, String> {
        let drive = DriveId::new(drive).ok_or(NO_DRIVE)?;
        let recorded = self.config.record_drive(&self.id, &drive).map_err(|e| e.to_string())?;
        if recorded != drive {
            tracing::warn!(
                "account {:?} is signed in to drive {drive}, but drive {recorded} is recorded for it",
                self.id
            );
        }
        Ok(recorded)
    }

    /// Asks Graph which drive this account is, and records it if none is recorded yet
    /// (§8.2: asked by another account's sign-in). The drive recorded.
    pub async fn learn_drive(&self) -> Result<DriveId, String> {
        let token = self.tokens.access_token().await.map_err(|e| e.to_string())?;
        let drive = self.graph().drive(&token).await.map_err(|e| e.to_string())?;
        self.record_drive(&drive.id)
    }

    /// Gets an access token for `refresh_account_info`, reconciling the visible state with
    /// what [`TokenManager::access_token`] found. Returns `Err(())` once the state (and, for
    /// a transient failure, `LastError`) has been dealt with, or there is nothing further to
    /// do because the token manager already handled it.
    async fn token_for_refresh(&self) -> Result<String, ()> {
        match self.tokens.access_token().await {
            Ok(token) => Ok(token),
            Err(AuthError::Transient(message)) => {
                self.state
                    .update(|s| s.set_error(format!("Could not load account info: {message}")));
                Err(())
            }
            // Wallet locked: the token manager already set `LastError` and the state is
            // still `SignedIn`, so a later unlock lets a retry succeed without user action.
            Err(AuthError::Locked) => Err(()),
            // `SignedOut` covers three different situations in the token manager:
            //   - invalid_grant: it already moved the state to `SignedOut` with
            //     `SESSION_EXPIRED`. The `still signed-in` guard below then does nothing,
            //     leaving that message alone.
            //   - no refresh token in the wallet, or
            //   - no client ID configured
            // For the latter two the state was never touched, so it is still `SignedIn`
            // here even though there is no way to actually refresh anything; that is the
            // "signed in with a lost wallet item" / "signed in with no client ID" bug this
            // guards against. Tell them apart from the account layer's own client ID.
            Err(AuthError::SignedOut) => {
                self.state.update(|s| {
                    if s.state == SignInState::SignedIn {
                        s.set_error(if s.client_id.is_empty() {
                            "Set a client ID first, then sign in again."
                        } else {
                            "The stored sign-in was lost. Sign in again."
                        });
                        s.state = SignInState::SignedOut;
                        s.clear_account();
                    }
                });
                Err(())
            }
        }
    }

    async fn finish_sign_in(&self, attempt: SignInAttempt) {
        let (generation, asked) = (attempt.generation, attempt.browser.scope());
        match self.complete_sign_in(attempt).await {
            Ok(tokens) => self.commit_sign_in(generation, asked, tokens).await,
            Err(message) => self.abort_sign_in(generation, Refused::Other(message)).await,
        }
    }

    /// Waits for the browser and exchanges the code, without touching shared state: the
    /// caller decides whether the result still applies.
    pub(super) async fn complete_sign_in(&self, attempt: SignInAttempt) -> Result<TokenResponse, String> {
        attempt.browser.tokens(self.sign_in_timeout, attempt.cancel).await
    }

    /// The other accounts the guard cannot tell apart from this drive ([`settle`]).
    async fn settle_siblings(&self) -> Vec<AccountId> {
        settle(self.siblings().map(|s| s.others(&self.id)).unwrap_or_default()).await
    }

    /// The identity guard's check and record (§8.2), in one `ConfigStore` update so that two
    /// sign-ins cannot pass it together: this slot is one drive, and a drive is one slot.
    /// `unsettled` are the other accounts that may be this drive without saying so
    /// ([`settle_siblings`](Self::settle_siblings)). `Err` says why the sign-in is refused;
    /// `Ok(true)` when the drive was recorded now, which [`unclaim`](Self::unclaim) undoes
    /// if the sign-in fails after all. The email the sign-in found, when there is one, is kept
    /// as the account's `login_hint` in the same write.
    fn claim(&self, drive: &DriveId, email: Option<&str>, unsettled: &[AccountId]) -> Result<bool, Refused> {
        let who = self.who();
        self.config.update(|config| -> Result<bool, Refused> {
            let mine = config.account(&self.id).ok_or_else(|| Refused::Other("This account was removed.".into()))?;
            if mine.drive_id.as_ref().is_some_and(|recorded| recorded != drive) {
                return Err(Refused::Other(format!(
                    "This account is {who}. You signed in as a different Microsoft account; to \
                     connect that one, add a new account."
                )));
            }
            drive_is_free(config, Some(&self.id), drive, unsettled)?;
            let mine = config.account_mut(&self.id).expect("found above");
            let recorded = mine.drive_id.is_none();
            mine.drive_id = Some(drive.clone());
            if let Some(email) = email.filter(|email| !email.is_empty()) {
                mine.login_hint = email.to_owned();
            }
            Ok(recorded)
        })
    }

    /// Takes back a drive [`claim`](Self::claim) recorded for a sign-in that then failed:
    /// a failed sign-in stores nothing (§8.2), and a slot that was never signed in must not
    /// keep the identity of the account it tried.
    fn unclaim(&self, drive: &DriveId) {
        let taken_back = self.config.update_account(&self.id, |account| {
            if account.drive_id.as_ref() == Some(drive) {
                account.drive_id = None;
            }
            Ok::<_, ConfigError>(())
        });
        if let Err(e) = taken_back {
            tracing::warn!("cannot take the drive of a failed sign-in back out of config.toml: {e}");
        }
    }

    /// Stores the refresh token and marks the session signed in, but only if `generation`
    /// is still the current attempt and the account still shows `signing-in`, and only once
    /// the identity guard (§8.2) has let this drive into this slot. Otherwise (a cancel or a
    /// sign-out ran first, a newer `begin_sign_in` superseded this one, something else ended
    /// the `signing-in` state, or the guard refused) the tokens are discarded
    /// without ever touching Secret Service. The guard is fail-closed: a drive that cannot
    /// be asked for refuses the sign-in too.
    async fn commit_sign_in(&self, generation: u64, asked: &'static str, tokens: TokenResponse) {
        let signed_in = match attempt::confirm(tokens, self.graph()).await {
            Ok(signed_in) => signed_in,
            Err(unconfirmed) => {
                self.abort_sign_in(generation, Refused::Other(unconfirmed.sentence())).await;
                return;
            }
        };
        let identity = &signed_in.identity;
        let unsettled = self.settle_siblings().await;
        let session = self.session.lock().await;
        // A retired account (`Accounts.Remove`) stores nothing, whatever the browser said.
        if session.generation != generation || self.is_retired() {
            return;
        }
        // Nor does an account that is not signing in: whatever ended that state said the
        // attempt had ended. Only a refresh that found the stored token dead does so without
        // superseding the attempt (limitations log F204), and nothing else says it.
        let state = self.state.get().state;
        if state != SignInState::SigningIn {
            tracing::warn!(
                "the sign-in of account {:?} is dropped: the browser answered, but the account was {state:?} by \
                 then, no longer signing in; signing in again works",
                self.id
            );
            return;
        }
        let recorded = match self.claim(&identity.drive, identity.email.as_deref(), &unsettled) {
            Ok(recorded) => recorded,
            Err(refused) => {
                drop(session);
                self.abort_sign_in(generation, refused).await;
                return;
            }
        };
        if let Err(e) = self.keep_sign_in(&signed_in, asked).await {
            if recorded {
                self.unclaim(&identity.drive);
            }
            drop(session);
            self.abort_sign_in(generation, Refused::Other(e)).await;
            return;
        }
        drop(session);
        self.refresh_account_info().await;
    }

    /// Stores the refresh token of `signed_in` and marks the session signed in with its
    /// tokens. Called with the session lock held, once the identity guard has let the drive
    /// into this slot. `Err`: the wallet did not take the token, and nothing changed.
    async fn keep_sign_in(&self, signed_in: &SignedIn, asked: &'static str) -> Result<(), String> {
        let SignedIn { tokens, refresh_token, identity } = signed_in;
        if let Some(email) = &identity.email {
            self.secrets.describe(email);
        }
        self.secrets.store(refresh_token).await.map_err(|e| e.to_string())?;
        self.tokens.seed_as(tokens, asked).await;
        self.state.update(|s| {
            s.state = SignInState::SignedIn;
            s.clear_error();
        });
        // What the sign-in granted, and the drive it reached, decide the mode (§7): a
        // read-write account that signed in again with Files.ReadWrite, to its own drive, is
        // read-write again.
        self.record_live_drive(&identity.drive);
        self.note_granted(asked, &tokens.granted(asked));
        Ok(())
    }

    /// `Accounts.SignIn`'s: this account was just made for the sign-in `signed_in`, whose
    /// drive the identity guard let in as the entry in `config.toml` was written
    /// ([`claim_new`]); it is signed in with it, as an account's own sign-in leaves it.
    /// `Err`: the wallet did not take the token, and the account is signed out.
    pub(crate) async fn start_signed_in(&self, signed_in: &SignedIn, asked: &'static str) -> Result<(), String> {
        let _session = self.session.lock().await;
        self.keep_sign_in(signed_in, asked).await
    }

    /// Records a sign-in failure, but only if `generation` is still the current attempt.
    /// An empty sentence means the user cancelled; `cancel_sign_in` already updated the
    /// state directly, and said so, so there is nothing left to do here.
    async fn abort_sign_in(&self, generation: u64, refused: Refused) {
        let message = refused.sentence();
        let session = self.session.lock().await;
        if session.generation != generation || message.is_empty() {
            return;
        }
        self.state.update(|s| {
            s.state = SignInState::SignedOut;
            s.set_error(message.clone());
        });
    }
}
