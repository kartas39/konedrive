use zbus::interface;

use crate::dbus::LocalScan;

#[interface(name = "org.konedrive.LocalScan")]
impl LocalScan {
    /// The Full local scan (issue #8): `running`, `idle`, or `none` for a read-only folder.
    #[zbus(property)]
    async fn state(&self) -> String {
        self.service.state().get().local.scan.state.as_str().to_owned()
    }

    /// Why the running (or the last) scan runs: start, read-write, helper-back, overflow,
    /// ignore-list, periodic.
    #[zbus(property)]
    async fn reason(&self) -> String {
        self.service.state().get().local.scan.reason
    }

    /// Unix seconds when it started; 0 before the first.
    #[zbus(property)]
    async fn started(&self) -> i64 {
        self.service.state().get().local.scan.started
    }

    /// Directories it has seen so far.
    #[zbus(property)]
    async fn directories(&self) -> u64 {
        self.service.state().get().local.scan.directories
    }

    /// Files (and other entries that are not directories) it has seen so far.
    #[zbus(property)]
    async fn files(&self) -> u64 {
        self.service.state().get().local.scan.files
    }

    /// About how many items it will see: the items the base had placed when it started.
    #[zbus(property)]
    async fn expected(&self) -> u64 {
        self.service.state().get().local.scan.expected
    }

    /// Unix seconds when the last scan finished; 0 for none since the daemon started.
    #[zbus(property)]
    async fn finished(&self) -> i64 {
        self.service.state().get().local.scan.finished
    }

    /// How long the last finished scan took, in seconds.
    #[zbus(property)]
    async fn took(&self) -> u32 {
        self.service.state().get().local.scan.took
    }
}
