//! `Accounts.HelperState`: how the privileged helper stands, as the daemon publishes it and
//! as a client reads it — the spellings, and what to tell a person in each state.

/// `Accounts.HelperState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HelperState {
    /// The daemon holds a link to the helper: files download when opened.
    Connected,
    /// systemd knows no `konedrive-helper.service`.
    NotInstalled,
    /// Installed, and not running.
    Stopped,
    /// The service failed, or its unit could not be loaded.
    Failed,
    /// No link, and systemd could not be asked — or says the helper runs,
    /// and the daemon is not connected to it yet.
    #[default]
    Unknown,
}

impl HelperState {
    /// Every state.
    pub const ALL: [HelperState; 5] = [Self::Connected, Self::NotInstalled, Self::Stopped, Self::Failed, Self::Unknown];

    /// The state as the property spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::NotInstalled => "not-installed",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    /// The state the property's value `text` spells; `None` for one this build does not know.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == text)
    }

    /// What to tell a person about the helper in each state that is not `connected`: what it
    /// means and how to start it. One wording for the daemon's `LastError` while a folder
    /// waits for the helper (HS3) and for `konedrivectl`'s `Helper:` line. `None` when
    /// connected.
    pub fn advice(self) -> Option<&'static str> {
        match self {
            Self::Connected => None,
            Self::NotInstalled => Some(
                "the konedrive helper is not installed: files are not kept in step and do not \
                 download when opened. Install it: sudo scripts/install-helper.sh (see README)",
            ),
            Self::Stopped => {
                Some("the konedrive helper is not running: start it with `sudo systemctl start konedrive-helper`")
            }
            Self::Failed => Some("the konedrive helper failed: see `systemctl status konedrive-helper`"),
            Self::Unknown => Some("the konedrive helper is not connected"),
        }
    }

    /// Whether the helper is known not to run: not installed, stopped or failed. Not so
    /// while nothing is known (`unknown`: systemd was not asked yet, or says it runs).
    pub fn known_down(self) -> bool {
        matches!(self, Self::NotInstalled | Self::Stopped | Self::Failed)
    }
}

#[cfg(test)]
mod tests;
