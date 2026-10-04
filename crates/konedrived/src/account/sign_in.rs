use super::*;

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
        self.state.update(|s| s.last_error.clear());
        let listener = match LoopbackListener::bind().await {
            Ok(listener) => listener,
            Err(e) => {
                let message = format!("cannot listen on localhost: {e}");
                self.state.update(|s| {
                    s.state = SignInState::SignedOut;
                    s.last_error = message.clone();
                });
                return Err(AccountError::Failed(message));
            }
        };
        // A read-write account signing in again asks for Files.ReadWrite again (§7).
        let oauth = self.oauth_client(&client_id, self.sign_in_mode());
        let redirect_uri = listener.redirect_uri();
        let pkce = Pkce::new();
        let csrf = random_token();
        let url = self.authorize_url(&oauth, &redirect_uri, &pkce, &csrf);
        let (cancel_tx, cancel_rx) = oneshot::channel();
        session.generation += 1;
        session.cancel = Some(cancel_tx);
        let generation = session.generation;
        drop(session);
        let attempt = SignInAttempt { oauth, listener, redirect_uri, pkce, csrf, cancel: cancel_rx, generation };
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

    /// The authorization URL of a sign-in with `oauth`: Microsoft's account picker for a
    /// read-only sign-in, so that any account can be chosen; pinned to this account whenever it
    /// asks for `Files.ReadWrite` — its password asked for again, with its email
    /// filled in when it is known: from the state, `account.json`, or — after a sign-out,
    /// which forgets both — the `login_hint` `config.toml` keeps.
    pub(super) fn authorize_url(&self, oauth: &OAuthClient, redirect_uri: &str, pkce: &Pkce, csrf: &str) -> String {
        if !grants_writes(oauth.scope()) {
            return oauth.picker_authorize_url(redirect_uri, pkce, csrf).to_string();
        }
        let email = [
            Some(self.state.get().email),
            crate::account::cache::load(&self.cache).map(|info| info.email),
            self.config.account(&self.id).map(|account| account.login_hint),
        ]
        .into_iter()
        .flatten()
        .find(|email| !email.is_empty());
        oauth.pinned_authorize_url(redirect_uri, pkce, csrf, email.as_deref()).to_string()
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
                s.last_error.clear();
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
                s.last_error.clear();
            }
        });
        // Takes TokenManager's own lock, so an in-flight access-token refresh commits its
        // rotated refresh token to the wallet before this deletes it.
        self.tokens.forget().await.map_err(|e| AccountError::Failed(e.to_string()))?;
        crate::account::cache::remove(&self.cache);
        self.state.update(|s| {
            s.state = SignInState::SignedOut;
            s.last_error.clear();
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
                    .update(|s| s.last_error = format!("Could not load account info: {e}"));
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
            info.fetched_at = unix_now();
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
                s.last_error.clear();
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
        self.recompute_mode();        // The folder's outbox decides by the quota just read whether OneDrive is still full.
        let uploads = self.uploads.lock().unwrap().as_ref().and_then(Weak::upgrade);
        if let Some(uploads) = uploads {
            uploads.quota_read(&drive.quota);
        }
    }

    /// The drive recorded for this account, if one is.
    fn recorded_drive(&self) -> Option<String> {
        self.config.account(&self.id).map(|a| a.drive_id).filter(|d| !d.is_empty())
    }

    /// Records `drive` as this account's when it has none yet; the drive recorded.
    fn record_drive(&self, drive: &str) -> Result<String, String> {
        if drive.is_empty() {
            return Err("Microsoft Graph did not say which drive this is".into());
        }
        let recorded = self.config.record_drive(&self.id, drive).map_err(|e| e.to_string())?;
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
    pub async fn learn_drive(&self) -> Result<String, String> {
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
                    .update(|s| s.last_error = format!("Could not load account info: {message}"));
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
                        s.last_error = if s.client_id.is_empty() {
                            "Set a client ID first, then sign in again.".into()
                        } else {
                            "The stored sign-in was lost. Sign in again.".into()
                        };
                        s.state = SignInState::SignedOut;
                        s.clear_account();
                    }
                });
                Err(())
            }
        }
    }

    async fn finish_sign_in(&self, attempt: SignInAttempt) {
        let (generation, asked) = (attempt.generation, attempt.oauth.scope());
        match self.complete_sign_in(attempt).await {
            Ok(tokens) => self.commit_sign_in(generation, asked, tokens).await,
            Err(message) => self.abort_sign_in(generation, message).await,
        }
    }

    /// Waits for the browser and exchanges the code, without touching shared state: the
    /// caller decides whether the result still applies.
    pub(super) async fn complete_sign_in(&self, attempt: SignInAttempt) -> Result<TokenResponse, String> {
        let SignInAttempt { oauth, listener, redirect_uri, pkce, csrf, cancel, .. } = attempt;
        let callback = tokio::select! {
            result = listener.wait(&csrf, self.sign_in_timeout) => result,
            _ = cancel => return Err(String::new()),
        };
        let code = match callback {
            Ok(Callback::Code(code)) => code,
            Ok(Callback::Error { error, .. }) if error == "access_denied" => {
                return Err("Access was denied in the browser.".into())
            }
            Ok(Callback::Error { error, description }) => {
                return Err(format!("Microsoft reported an error: {error}: {description}"))
            }
            Err(LoopbackError::TimedOut) => {
                return Err("Timed out waiting for the browser. If Microsoft showed an error page, \
                            check the app registration: client ID and redirect URI http://localhost."
                    .into())
            }
            Err(e) => return Err(e.to_string()),
        };
        oauth
            .exchange_code(&code, &pkce.verifier, &redirect_uri)
            .await
            .map_err(|e| format!("Sign-in failed: {e}"))
    }

    /// Which drive — and whose email — the new access token is for (§8.2). Only the drive
    /// is needed: it is the check. The email names the wallet item.
    pub(super) async fn identify(&self, access_token: &str) -> Result<Identity, String> {
        let graph = self.graph();
        let (drive, profile) = tokio::join!(graph.drive(access_token), graph.profile(access_token));
        let drive = drive.map_err(|e| e.to_string())?;
        if drive.id.is_empty() {
            return Err("Microsoft Graph did not say which drive this is".into());
        }
        Ok(Identity { drive: drive.id, email: profile.ok().map(|p| p.email).filter(|e| !e.is_empty()) })
    }

    /// The other accounts the guard cannot tell apart from this drive (§8.2), by id: those
    /// with no drive recorded that may be signed in to one — signed in, or holding a refresh
    /// token they have not used yet (a wallet that did not answer at startup leaves an
    /// account signed out with its token still stored). Each is asked for its drive first,
    /// and is left out once it has one.
    async fn settle_siblings(&self) -> Vec<String> {
        let others = self.siblings().map(|s| s.others(&self.id)).unwrap_or_default();
        let mut unsettled = Vec::new();
        for other in others {
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

    /// The identity guard's check and record (§8.2), in one `ConfigStore` update so that two
    /// sign-ins cannot pass it together: this slot is one drive, and a drive is one slot.
    /// `unsettled` are the other accounts that may be this drive without saying so
    /// ([`settle_siblings`](Self::settle_siblings)). `Err` says why the sign-in is refused;
    /// `Ok(true)` when the drive was recorded now, which [`unclaim`](Self::unclaim) undoes
    /// if the sign-in fails after all. The email the sign-in found, when there is one, is kept
    /// as the account's `login_hint` in the same write.
    fn claim(&self, drive: &str, email: Option<&str>, unsettled: &[String]) -> Result<bool, String> {
        let who = {
            let s = self.state.get();
            if s.email.is_empty() {
                format!("'{}'", s.label)
            } else {
                s.email
            }
        };
        self.config.update(|config| -> Result<bool, String> {
            let mine = config.account(&self.id).ok_or_else(|| "This account was removed.".to_owned())?;
            if !mine.drive_id.is_empty() && mine.drive_id != drive {
                return Err(format!(
                    "This account is {who}. You signed in as a different Microsoft account; to \
                     connect that one, add a new account."
                ));
            }
            if let Some(other) = config.accounts.iter().find(|a| a.id != self.id && a.drive_id == drive) {
                return Err(format!("This Microsoft account is already connected as '{}'.", other.label));
            }
            if config.accounts.iter().any(|a| a.id != self.id && a.drive_id.is_empty() && unsettled.contains(&a.id)) {
                return Err("Could not check which account this is; try again.".into());
            }
            let mine = config.account_mut(&self.id).expect("found above");
            let recorded = mine.drive_id.is_empty();
            mine.drive_id = drive.to_owned();
            if let Some(email) = email.filter(|email| !email.is_empty()) {
                mine.login_hint = email.to_owned();
            }
            Ok(recorded)
        })
    }

    /// Takes back a drive [`claim`](Self::claim) recorded for a sign-in that then failed:
    /// a failed sign-in stores nothing (§8.2), and a slot that was never signed in must not
    /// keep the identity of the account it tried.
    fn unclaim(&self, drive: &str) {
        let taken_back = self.config.update_account(&self.id, |account| {
            if account.drive_id == drive {
                account.drive_id.clear();
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
        let Some(refresh_token) = tokens.refresh_token.clone() else {
            self.abort_sign_in(generation, "Microsoft did not return a refresh token.".into()).await;
            return;
        };
        let identity = match self.identify(&tokens.access_token).await {
            Ok(identity) => identity,
            Err(e) => {
                let message = format!("Could not check which account this is ({e}); try again.");
                self.abort_sign_in(generation, message).await;
                return;
            }
        };
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
            Err(message) => {
                drop(session);
                self.abort_sign_in(generation, message).await;
                return;
            }
        };
        if let Some(email) = &identity.email {
            self.secrets.describe(email);
        }
        if let Err(e) = self.secrets.store(&refresh_token).await {
            if recorded {
                self.unclaim(&identity.drive);
            }
            drop(session);
            self.abort_sign_in(generation, e.to_string()).await;
            return;
        }
        self.tokens.seed_as(&tokens, asked).await;
        self.state.update(|s| {
            s.state = SignInState::SignedIn;
            s.last_error.clear();
        });
        // What the sign-in granted, and the drive it reached, decide the mode (§7): a
        // read-write account that signed in again with Files.ReadWrite, to its own drive, is
        // read-write again.
        self.record_live_drive(&identity.drive);
        self.note_granted(asked, &tokens.granted(asked));
        drop(session);
        self.refresh_account_info().await;
    }

    /// Records a sign-in failure, but only if `generation` is still the current attempt.
    /// An empty `message` means the user cancelled; `cancel_sign_in` already updated the
    /// state directly, so there is nothing left to do here.
    async fn abort_sign_in(&self, generation: u64, message: String) {
        let session = self.session.lock().await;
        if session.generation != generation || message.is_empty() {
            return;
        }
        self.state.update(|s| {
            s.state = SignInState::SignedOut;
            s.last_error = message.clone();
        });
    }
}
