use super::*;
use crate::account::attempt::Unconfirmed;

impl AccountService {
    /// `Account.SetMode` (`docs/design/writes.md` §2): switches the account to `mode` — `read-only` or
    /// `read-write` — and answers the URL of the sign-in the switch needs, empty when it
    /// needs none.
    ///
    /// - **To read-write**: any account may be, whatever its drive: it is the user's choice.
    ///   Refused `NotSignedIn` unless the account is signed
    ///   in, and `Failed` when it has no drive recorded and Graph does not say now which one
    ///   it is. Nothing is written yet: a sign-in asking for `Files.ReadWrite` begins, and only
    ///   when its token response grants that — for this account's own drive —
    ///   are the refresh token stored and `mode = "read-write"` written; the
    ///   folder then follows `Mode`. A cancelled, refused or failed sign-in changes nothing
    ///   and says why in `LastError`; the account stays signed in, read-only. An account
    ///   read-write already needs no sign-in.
    /// - **To read-only**: refused `PendingUploads` while changes wait to be uploaded, unless
    ///   `force`, which drops them (the files stay, as ordinary local changes). Then
    ///   `mode = "read-only"` is written, the token that could write is dropped, and the next
    ///   refresh asks for `Files.Read`, a subset of the grant: no sign-in. A switch to
    ///   read-write still waiting for its browser is given up.
    pub async fn set_mode(self: &Arc<Self>, mode: &str, force: bool) -> Result<String, ModeError> {
        let mode = Mode::parse(mode)
            .ok_or_else(|| ModeError::InvalidMode(format!("{mode:?} is not a mode: read-only or read-write")))?;
        if self.is_retired() {
            return Err(ModeError::Failed(RETIRED.into()));
        }
        match mode {
            Mode::ReadWrite => self.switch_to_read_write().await,
            Mode::ReadOnly => self.switch_to_read_only(force).await.map(|()| String::new()),
        }
    }

    /// [`set_mode`](Self::set_mode) to read-write: the sign-in it needs.
    async fn switch_to_read_write(self: &Arc<Self>) -> Result<String, ModeError> {
        let not_signed_in = || ModeError::NotSignedIn("sign in first; then switch the account to read-write".into());
        let snapshot = self.state.get();
        if snapshot.state != SignInState::SignedIn {
            return Err(not_signed_in());
        }
        if snapshot.mode == Mode::ReadWrite {
            return Ok(String::new());
        }
        if snapshot.client_id.is_empty() {
            return Err(ModeError::Failed(AccountError::NoClientId.to_string()));
        }
        // The sign-in is taken only for the account's recorded drive: with none recorded it
        // could only fail, after the trip through the browser. So the drive is asked for now,
        // and the switch refused before any URL when it cannot be recorded.
        if self.config.account(&self.id).is_some_and(|mine| mine.drive_id.is_none()) {
            if let Err(e) = self.learn_drive().await {
                return Err(ModeError::Failed(format!("{DRIVE_NOT_KNOWN} ({e})")));
            }
        }
        let browser = Attempt::bind(self.oauth_client(&snapshot.client_id, Mode::ReadWrite)).await.map_err(ModeError::Failed)?;
        // Pinned to this account: the password asked for again, its email filled
        // in, so that a browser signed in to another account cannot consent for it.
        let url = self.authorize_url(&browser);
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let generation = {
            let mut session = self.session.lock().await;
            // Asked again under the lock: a sign-out and a new sign-in in the
            // meantime must not have their attempt cancelled by this switch.
            if self.state.get().state != SignInState::SignedIn {
                return Err(not_signed_in());
            }
            // A switch still waiting for its browser gives way to this one.
            session.generation += 1;
            if let Some(previous) = session.cancel.replace(cancel_tx) {
                let _ = previous.send(());
            }
            self.state.update(|s| s.clear_error());
            session.generation
        };
        let attempt = SignInAttempt { browser, cancel: cancel_rx, generation };
        let this = Arc::clone(self);
        tokio::spawn(async move { this.finish_read_write(attempt).await });
        Ok(url)
    }

    /// The browser's answer to a switch to read-write, and what it leads to.
    async fn finish_read_write(&self, attempt: SignInAttempt) {
        let (generation, asked) = (attempt.generation, attempt.browser.scope());
        match self.complete_sign_in(attempt).await {
            Ok(tokens) => self.commit_read_write(generation, asked, tokens).await,
            Err(message) => self.abort_read_write(generation, message).await,
        }
    }

    /// The switch to read-write, once its token response is in: only a grant of
    /// `Files.ReadWrite`, for this account's own drive (`GET /me/drive` with the new token),
    /// is taken. Then the refresh token is stored, the new token
    /// cached and what it was granted recorded, and only then `mode = "read-write"` written
    /// and the mode worked out again. Anything else changes nothing but
    /// `LastError`.
    async fn commit_read_write(&self, generation: u64, asked: &'static str, tokens: TokenResponse) {
        let granted = tokens.granted(asked);
        if !grants_writes(&granted) {
            let message = format!(
                "Microsoft did not allow konedrive to change files in OneDrive (the sign-in granted \
                 {granted:?}); the account stays read-only."
            );
            return self.abort_read_write(generation, message).await;
        }
        let SignedIn { tokens, refresh_token, identity } = match attempt::confirm(tokens, self.graph()).await {
            Ok(signed_in) => signed_in,
            Err(Unconfirmed::NoRefreshToken) => {
                return self.abort_read_write(generation, "Microsoft did not return a refresh token; the account stays read-only.".into()).await;
            }
            Err(Unconfirmed::Unidentified(e)) => {
                let message = format!("Could not check which account this is ({e}); the account stays read-only.");
                return self.abort_read_write(generation, message).await;
            }
        };
        let session = self.session.lock().await;
        if session.generation != generation || self.is_retired() || self.state.get().state != SignInState::SignedIn {
            return;
        }
        let who = self.who();
        let now = self.config.current();
        let refusal = match &now {
            Some(config) => read_write_refusal(config, &self.id, &identity.drive, &who),
            None => Some(CONFIG_UNREADABLE_AT_SWITCH.into()),
        };
        if let Some(why) = refusal {
            drop(session);
            return self.abort_read_write(generation, why).await;
        }
        // The token first: one that can write, for this very drive, is harmless under a
        // read-only mode (every refresh asks for Files.Read then), and the mode is written
        // only once it is stored.
        if let Err(e) = self.secrets.store(&refresh_token).await {
            drop(session);
            return self.abort_read_write(generation, format!("{e}; the account stays read-only.")).await;
        }
        // All with the token manager's refresh lock held: no refresh can run between recording the
        // grant and caching the new token, so none can record its narrower grant over it. The
        // grant and the drive are recorded before config.toml says read-write, so
        // a crash in between finds them; the new token is cached only once it does, and the
        // mode is worked out again under the same hold.
        let email = identity.email.clone().unwrap_or_default();
        let written = self
            .tokens
            .commit_as(&tokens, asked, || {
                self.record_granted(asked, &granted);
                self.record_live_drive(&identity.drive);
                // The check again, and the write it allows, in one step.
                let written = self.config.update(|config| match read_write_refusal(config, &self.id, &identity.drive, &who) {
                    Some(why) => Err(why),
                    None => {
                        let mine = config.account_mut(&self.id).expect("checked above");
                        mine.mode = Mode::ReadWrite;
                        if !email.is_empty() {
                            mine.login_hint = email.clone();
                        }
                        Ok(())
                    }
                });
                if written.is_ok() {
                    self.state.update(|s| s.clear_error());
                }
                self.recompute_mode();
                written
            })
            .await;
        if let Err(why) = written {
            drop(session);
            return self.abort_read_write(generation, why).await;
        }
        tracing::info!("the account {:?} is read-write now", self.id);
    }

    /// A switch to read-write that did not go through: `LastError` says why, and nothing
    /// else changes — the account stays signed in, read-only. Only for the current attempt;
    /// an empty `message` is a cancel, which says nothing.
    async fn abort_read_write(&self, generation: u64, message: String) {
        let session = self.session.lock().await;
        if session.generation != generation || message.is_empty() {
            return;
        }
        self.state.update(|s| s.set_error(message));
    }

    /// [`set_mode`](Self::set_mode) to read-only.
    async fn switch_to_read_only(&self, force: bool) -> Result<(), ModeError> {
        {
            // A switch to read-write still waiting for its browser is given up. Signing in has
            // its own attempt, which this leaves alone: the state is asked under the lock.
            let mut session = self.session.lock().await;
            if self.state.get().state == SignInState::SignedIn {
                session.generation += 1;
                if let Some(cancel) = session.cancel.take() {
                    let _ = cancel.send(());
                }
            }
        }
        let uploads = crate::panic::lock(&self.uploads).as_ref().and_then(Weak::upgrade);
        if self.configured_mode() == Mode::ReadOnly {
            // Read-only already, changes may still wait: a switch nobody forced kept them.
            // Forced, they go now — the way out a Forget and a Remove refused
            // `PendingUploads` point to.
            if let (true, Some(uploads)) = (force, &uploads) {
                uploads.drop_pending_uploads().await;
            }
            self.recompute_mode();
            return Ok(());
        }
        if let Some(uploads) = &uploads {
            let pending = uploads.pending_uploads().await;
            if pending > 0 && !force {
                return Err(ModeError::PendingUploads(format!(
                    "{pending} changes made here have not been uploaded yet; wait for them, or force \
                     the switch: they then stay here, and are not uploaded"
                )));
            }
        }
        self.config
            .update_account(&self.id, |account| {
                account.mode = Mode::ReadOnly;
                Ok::<_, ConfigError>(())
            })
            .map_err(|e| ModeError::Failed(format!("cannot save the configuration: {e}")))?;
        // Dropped only once config.toml says read-only: a write that failed leaves
        // the account read-write with its rows.
        if let (true, Some(uploads)) = (force, &uploads) {
            uploads.drop_pending_uploads().await;
        }
        self.recompute_mode();
        // The token that could write goes now; the next one comes from a refresh that asks
        // for Files.Read (§7).
        self.tokens.invalidate().await;
        tracing::info!("the account {:?} is read-only now", self.id);
        Ok(())
    }

    /// The account's recorded drive when `write_test_drive_ids` lists it, both from one
    /// reading of `config.toml` as it is now: the one thing the list decides.
    fn exportable_drive(&self) -> Option<DriveId> {
        let config = self.config.current()?;
        let drive = config.account(&self.id)?.drive_id.clone()?;
        config.read_write_export_allowed(&drive).then_some(drive)
    }

    /// `TokenExport.ReadOnly` (`docs/design/writes.md` §8.2; SECURITY.md): a token that can change nothing, whatever the
    /// account's mode.
    pub async fn read_only_token(&self) -> Result<String, AuthError> {
        self.tokens.read_only_token().await
    }

    /// `TokenExport.ReadWrite`, for the test-account harness only (`docs/design/writes.md` §2.3, §12.1; SECURITY.md):
    /// refused `WritesNotAllowed` unless `write_test_drive_ids` lists the account's drive, and
    /// `ModeNotGranted` unless the account is read-write and its token carries
    /// `Files.ReadWrite`. The token itself is then asked which drive it reaches
    /// (`GET /me/drive`): another than the listed one refuses it
    /// `WritesNotAllowed`, and turns the account read-only.
    pub async fn read_write_token(&self) -> Result<String, ModeError> {
        let Some(drive) = self.exportable_drive() else {
            return Err(ModeError::WritesNotAllowed(EXPORT_NOT_ALLOWED.into()));
        };
        let not_granted = || ModeError::ModeNotGranted("this account is read-only: switch it to read-write first".into());
        if self.mode() != Mode::ReadWrite {
            return Err(not_granted());
        }
        let token = match self.tokens.access_token_and_scope().await {
            Ok((token, scope)) if grants_writes(&scope) => token,
            Ok(_) => return Err(not_granted()),
            Err(AuthError::SignedOut) => return Err(ModeError::NotSignedIn("nobody is signed in".into())),
            Err(e) => return Err(ModeError::Failed(e.to_string())),
        };
        let live = self
            .graph()
            .drive(&token)
            .await
            .map_err(|e| ModeError::Failed(format!("cannot check which drive the token reaches: {e}")))?
            .id;
        self.record_live_drive(&live);
        if drive != live {
            self.recompute_mode();
            return Err(ModeError::WritesNotAllowed(format!(
                "the account's token reaches drive {live}, not drive {drive}, which config.toml records \
                 and write_test_drive_ids lists; it is not handed out, and the account runs read-only"
            )));
        }
        Ok(token)
    }
}

/// Why a switch to read-write that reached `drive` is refused, if it is: the drive must be
/// the account's own: a sign-in as someone else changes nothing. Nothing else is asked of it.
/// An account with no drive recorded (the switch records one before its sign-in begins, so
/// only a `config.toml` that lost it since) has none to compare with, and is told that.
fn read_write_refusal(config: &Config, id: &AccountId, drive: &DriveId, who: &str) -> Option<String> {
    let Some(mine) = config.account(id) else { return Some("This account was removed.".into()) };
    let Some(recorded) = &mine.drive_id else {
        return Some(format!("{DRIVE_NOT_KNOWN}; the account stays read-only."));
    };
    if recorded != drive {
        return Some(format!(
            "This account is {who}, and the browser signed in as a different Microsoft account; the \
             account stays read-only."
        ));
    }
    None
}
