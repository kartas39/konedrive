use std::path::Path;

use zbus::object_server::SignalEmitter;
use zbus::interface;

use crate::dbus::Folder;
use crate::dbus::fault::{Fault, Result, to_fault};

#[interface(name = "org.konedrive.Folder")]
impl Folder {
    async fn register(&self, path: &str) -> Result<()> {
        self.service.register_root(Path::new(path)).await.map_err(to_fault)
    }

    /// Binds a folder with **nothing intercepting opens inside it**.
    /// Separate from `Register`, rather than a flag on it, so
    /// that nobody enters this mode without naming it: a placeholder nobody
    /// intercepts reads as zeros, which `State = no-interception` and
    /// `LastError` then say in as many words.
    async fn register_without_interception(&self, path: &str) -> Result<()> {
        self.service
            .register_root_without_interception(Path::new(path))
            .await
            .map_err(to_fault)
    }

    async fn unregister(&self) -> Result<()> {
        self.service.unregister_root().await.map_err(to_fault)
    }

    /// Mirrors a local directory as placeholders. Part 2 replaces the source
    /// with the Graph listing; this stays as the offline test path.
    async fn populate_from_directory(&self, source_dir: &str) -> Result<u64> {
        self.service.populate_from_directory(Path::new(source_dir)).await.map_err(to_fault)
    }

    async fn refresh(&self) -> Result<()> {
        self.service.refresh().await.map_err(to_fault)
    }

    async fn skipped(&self) -> Result<Vec<(String, String)>> {
        self.service.skipped().await.map_err(to_fault)
    }

    #[zbus(out_args("files", "bytes", "busy"))]
    async fn free_up_space(&self) -> Result<(u32, u64, u32)> {
        let freed = self.service.free_up_space().await.map_err(to_fault)?;
        Ok((freed.files, freed.bytes, freed.busy))
    }

    /// Nothing is uploaded, and OneDrive is not asked for changes, for
    /// `seconds` — or until `Resume()` when 0.
    async fn pause(&self, seconds: u32) -> Result<()> {
        self.service.pause_syncing(seconds).await.map_err(to_fault)
    }

    async fn resume(&self) -> Result<()> {
        self.service.resume_syncing().await.map_err(to_fault)
    }

    /// The account's ignore list from now on; a Full local scan follows.
    async fn set_ignore_patterns(
        &self,
        patterns: Vec<String>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<()> {
        self.service.set_ignore_patterns(patterns).await.map_err(to_fault)?;
        self.ignore_patterns_changed(&emitter).await.map_err(Fault::ZBus)
    }

    /// Whether Graph's thumbnails of images and videos are fetched; written to `config.toml`.
    async fn set_thumbnails(&self, on: bool, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> Result<()> {
        self.service.change_run_settings(move |s| s.thumbnails = on).await.map_err(to_fault)?;
        self.thumbnails_changed(&emitter).await.map_err(Fault::ZBus)
    }

    /// Lifts the automatic hold now, until a source or the global `Accounts.PauseOnMetered` /
    /// `OnBattery` changes.
    async fn sync_anyway(&self) -> Result<()> {
        self.service.sync_anyway().map_err(to_fault)
    }

    /// Why the account holds back by itself now: `metered`, `on-battery`, `power-saver`, or
    /// empty.
    #[zbus(property)]
    async fn held_back(&self) -> String {
        self.service.state().get().held_back
    }

    /// How changes made in OneDrive reach this computer: `connected` (at once, through the
    /// notification socket), `connecting` (trying; the poll runs meanwhile), or `off`
    /// (stopped, or not a OneDrive folder).
    #[zbus(property)]
    async fn live_changes(&self) -> String {
        self.service.state().get().live_changes.as_str().to_owned()
    }

    #[zbus(property)]
    async fn thumbnails(&self) -> bool {
        self.service.run_settings().thumbnails
    }

    #[zbus(property)]
    /// From the published state, as `State` and `LastError` are: a
    /// folder that could not be brought up reads
    /// `error` and still says which folder it is.
    async fn path(&self) -> String {
        self.service.state().get().root_path
    }

    #[zbus(property)]
    async fn state(&self) -> String {
        self.service.root_state()
    }

    #[zbus(property)]
    async fn last_error(&self) -> String {
        self.service.last_error()
    }

    #[zbus(property)]
    async fn source(&self) -> String {
        self.service.root_source()
    }

    #[zbus(property)]
    async fn items_listed(&self) -> u64 {
        self.service.items().0
    }

    #[zbus(property)]
    async fn items_placed(&self) -> u64 {
        self.service.items().1
    }

    #[zbus(property)]
    async fn skipped_count(&self) -> u64 {
        self.service.items().2
    }

    #[zbus(property)]
    async fn last_checked(&self) -> i64 {
        self.service.status().0
    }

    #[zbus(property)]
    async fn local_bytes(&self) -> u64 {
        self.service.status().1
    }

    /// Files and folders with a pin of their own.
    #[zbus(property)]
    async fn pinned_count(&self) -> u32 {
        self.service.pinned_count()
    }

    #[zbus(property)]
    async fn ignore_patterns(&self) -> Vec<String> {
        self.service.ignore_patterns()
    }

    #[zbus(property)]
    async fn paused(&self) -> bool {
        self.service.state().get().paused_until.is_some()
    }

    /// Unix seconds; 0 while paused until resumed, and while not paused.
    #[zbus(property)]
    async fn paused_until(&self) -> i64 {
        self.service.state().get().paused_until.unwrap_or(0)
    }
}
