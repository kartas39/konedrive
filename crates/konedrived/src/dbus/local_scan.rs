use zbus::interface;

use crate::dbus::properties::*;
use crate::dbus::LocalScan;

/// The Full local scan. Every property is a row of `properties`, which says
/// what it is.
#[interface(name = "org.konedrive.LocalScan")]
impl LocalScan {
    #[zbus(property)]
    async fn state(&self) -> String {
        SCAN_STATE.of(&self.service)
    }

    #[zbus(property)]
    async fn reason(&self) -> String {
        SCAN_REASON.of(&self.service)
    }

    #[zbus(property)]
    async fn started(&self) -> i64 {
        SCAN_STARTED.of(&self.service)
    }

    #[zbus(property)]
    async fn directories(&self) -> u64 {
        SCAN_DIRECTORIES.of(&self.service)
    }

    #[zbus(property)]
    async fn files(&self) -> u64 {
        SCAN_FILES.of(&self.service)
    }

    #[zbus(property)]
    async fn expected(&self) -> u64 {
        SCAN_EXPECTED.of(&self.service)
    }

    #[zbus(property)]
    async fn finished(&self) -> i64 {
        SCAN_FINISHED.of(&self.service)
    }

    #[zbus(property)]
    async fn took(&self) -> u32 {
        SCAN_TOOK.of(&self.service)
    }
}
