//! The privileged helper as the daemon sees it — `Sync1.HelperState` (HS1).
//!
//! While the daemon holds a link to the helper, the helper is `connected`.
//! Otherwise systemd is asked, read-only and over the system bus, how its
//! `konedrive-helper.service` stands, so that the window and `konedrivectl
//! sync status` can say what to do: install it, start it, or look at why it
//! failed. Asking needs no privilege: `LoadUnit` and the unit's properties are
//! open to every user, as `systemctl status` shows.
//!
//! The question goes through [`HelperUnit`], so that a test answers it with a
//! fake: no test may ever reach the real system bus.

use std::time::Duration;

use async_trait::async_trait;

/// The helper's systemd unit.
pub const HELPER_UNIT: &str = "konedrive-helper.service";

/// How often systemd is asked again while there is no link (HS1).
pub const RECHECK: Duration = Duration::from_secs(30);

/// `Sync1.HelperState`.
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
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::NotInstalled => "not-installed",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    /// What `LastError` says while a folder waits for the helper (HS3): what
    /// the state means and how to start the helper. `None` when connected.
    pub fn advice(self) -> Option<&'static str> {
        konedrive_dbus::helper_advice(self.as_str())
    }

    /// The state systemd's `LoadState` and `ActiveState` of the unit stand
    /// for, when the daemon has no link.
    pub fn of_unit(load: &str, active: &str) -> Self {
        match (load, active) {
            ("not-found", _) => Self::NotInstalled,
            (_, "failed") | ("error" | "bad-setting", _) => Self::Failed,
            (_, "inactive" | "deactivating") => Self::Stopped,
            _ => Self::Unknown,
        }
    }
}

/// How the helper's unit stands, as systemd says.
#[async_trait]
pub trait HelperUnit: Send + Sync {
    /// The unit's `LoadState` and `ActiveState`; `None` when systemd cannot
    /// be asked.
    async fn states(&self) -> Option<(String, String)>;
}

/// Asks nobody, and so knows nothing: what a `SyncService` starts with, so
/// that a test that never sets a unit never reaches the system bus. Only
/// `main` installs [`Systemd`].
pub struct NotAsked;

#[async_trait]
impl HelperUnit for NotAsked {
    async fn states(&self) -> Option<(String, String)> {
        None
    }
}

/// systemd itself, on the system bus: `Manager.LoadUnit`, then the unit's
/// `LoadState` and `ActiveState`. A unit systemd has no file for comes back
/// `not-found` (or, from an older systemd, as `NoSuchUnit`).
pub struct Systemd {
    pub unit: String,
    /// The most one question may take; a bus that does not answer is
    /// `unknown`, not a stuck daemon.
    pub timeout: Duration,
}

impl Default for Systemd {
    fn default() -> Self {
        Self { unit: HELPER_UNIT.to_owned(), timeout: Duration::from_secs(5) }
    }
}

#[async_trait]
impl HelperUnit for Systemd {
    async fn states(&self) -> Option<(String, String)> {
        match tokio::time::timeout(self.timeout, self.ask()).await {
            Ok(Ok(states)) => Some(states),
            Ok(Err(e)) => {
                tracing::info!("cannot ask systemd about {}: {e}", self.unit);
                None
            }
            Err(_) => {
                tracing::info!("systemd did not answer about {} within {:?}", self.unit, self.timeout);
                None
            }
        }
    }
}

impl Systemd {
    async fn ask(&self) -> zbus::Result<(String, String)> {
        const SYSTEMD: &str = "org.freedesktop.systemd1";
        let connection = zbus::Connection::system().await?;
        let loaded = connection
            .call_method(Some(SYSTEMD), "/org/freedesktop/systemd1", Some("org.freedesktop.systemd1.Manager"), "LoadUnit", &(self.unit.as_str(),))
            .await;
        let reply = match loaded {
            Ok(reply) => reply,
            Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.systemd1.NoSuchUnit" => {
                return Ok(("not-found".to_owned(), "inactive".to_owned()));
            }
            Err(e) => return Err(e),
        };
        let path: zbus::zvariant::OwnedObjectPath = reply.body().deserialize()?;
        let properties = zbus::fdo::PropertiesProxy::builder(&connection)
            .destination(SYSTEMD)?
            .path(path)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        let unit = zbus::names::InterfaceName::from_static_str_unchecked("org.freedesktop.systemd1.Unit");
        let text = |value: zbus::zvariant::OwnedValue| String::try_from(value).map_err(zbus::Error::from);
        let load = text(properties.get(unit.clone(), "LoadState").await?)?;
        let active = text(properties.get(unit, "ActiveState").await?)?;
        Ok((load, active))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemds_states_map_to_the_helpers() {
        assert_eq!(HelperState::of_unit("not-found", "inactive"), HelperState::NotInstalled);
        assert_eq!(HelperState::of_unit("loaded", "inactive"), HelperState::Stopped);
        assert_eq!(HelperState::of_unit("loaded", "deactivating"), HelperState::Stopped);
        assert_eq!(HelperState::of_unit("loaded", "failed"), HelperState::Failed);
        assert_eq!(HelperState::of_unit("bad-setting", "inactive"), HelperState::Failed);
        assert_eq!(HelperState::of_unit("loaded", "active"), HelperState::Unknown, "running, and no link yet");
        assert_eq!(HelperState::Connected.advice(), None);
        assert!(HelperState::Stopped.advice().unwrap().contains("sudo systemctl start konedrive-helper"));
    }
}
