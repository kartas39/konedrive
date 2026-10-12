use konedrive_dbus::rows::Conflict;
use zbus::interface;

use crate::dbus::Conflicts;
use crate::dbus::fault::Result;
use crate::dbus::properties::CONFLICT_COUNT;

/// The fourth field of `Conflicts.List()`: a local version is kept as a copy beside the
/// file, and in no other way.
const KEPT_AS_COPY: &str = "copy";

#[interface(name = "org.konedrive.Conflicts")]
impl Conflicts {
    /// (unix time, original full path, full path of the kept copy, how it was kept:
    /// `copy`, the one way there is), newest first; one whose kept copy is gone is
    /// dropped.
    async fn list(&self) -> Result<Vec<Conflict>> {
        let rows = self.service.conflicts().await?;
        Ok(rows.into_iter().map(|c| Conflict { at: c.at, original: c.original, kept: c.kept_copy, how: KEPT_AS_COPY.to_owned() }).collect())
    }

    async fn dismiss(&self, kept_path: &str) -> Result<()> {
        Ok(self.service.dismiss_conflict(kept_path).await?)
    }

    #[zbus(property)]
    async fn count(&self) -> u32 {
        CONFLICT_COUNT.of(&self.service)
    }

    #[zbus(property)]
    async fn machine_name(&self) -> String {
        self.service.machine_name()
    }
}
