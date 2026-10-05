use std::path::Path;

use zbus::object_server::SignalEmitter;
use zbus::interface;

use crate::dbus::Folder;
use crate::dbus::fault::{Fault, Result};
use crate::dbus::properties::*;

#[interface(name = "org.konedrive.Folder")]
impl Folder {
    async fn register(&self, path: &str) -> Result<()> {
        Ok(self.service.register_root(Path::new(path)).await?)
    }

    /// Binds a folder with **nothing intercepting opens inside it**.
    /// Separate from `Register`, rather than a flag on it, so
    /// that nobody enters this mode without naming it: a placeholder nobody
    /// intercepts reads as zeros, which `State = no-interception` and
    /// `LastError` then say in as many words.
    async fn register_without_interception(&self, path: &str) -> Result<()> {
        Ok(self.service.register_root_without_interception(Path::new(path)).await?)
    }

    async fn unregister(&self) -> Result<()> {
        Ok(self.service.unregister_root().await?)
    }

    /// Mirrors a local directory as placeholders. Part 2 replaces the source
    /// with the Graph listing; this stays as the offline test path.
    async fn populate_from_directory(&self, source_dir: &str) -> Result<u64> {
        Ok(self.service.populate_from_directory(Path::new(source_dir)).await?)
    }

    async fn refresh(&self) -> Result<()> {
        Ok(self.service.refresh().await?)
    }

    async fn skipped(&self) -> Result<Vec<(String, String, String, String)>> {
        Ok(self.service.skipped().await?)
    }

    #[zbus(out_args("files", "bytes", "busy"))]
    async fn free_up_space(&self) -> Result<(u32, u64, u32)> {
        let freed = self.service.free_up_space().await?;
        Ok((freed.files, freed.bytes, freed.busy))
    }

    /// Nothing is uploaded, and OneDrive is not asked for changes, for
    /// `seconds` — or until `Resume()` when 0.
    async fn pause(&self, seconds: u32) -> Result<()> {
        Ok(self.service.pause_syncing(seconds).await?)
    }

    async fn resume(&self) -> Result<()> {
        Ok(self.service.resume_syncing().await?)
    }

    /// The account's ignore list from now on; a Full local scan follows.
    async fn set_ignore_patterns(
        &self,
        patterns: Vec<String>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<()> {
        self.service.set_ignore_patterns(patterns).await?;
        self.ignore_patterns_changed(&emitter).await.map_err(Fault::ZBus)
    }

    /// Whether Graph's thumbnails of images and videos are fetched; written to `config.toml`.
    async fn set_thumbnails(&self, on: bool, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> Result<()> {
        self.service.change_run_settings(move |s| s.thumbnails = on).await?;
        self.thumbnails_changed(&emitter).await.map_err(Fault::ZBus)
    }

    /// Lifts the automatic hold now, until a source or the global `Accounts.PauseOnMetered` /
    /// `OnBattery` changes.
    async fn sync_anyway(&self) -> Result<()> {
        Ok(self.service.sync_anyway()?)
    }

    // The properties the daemon announces by itself: each a row of `properties`, which
    // says what it is.

    #[zbus(property)]
    async fn path(&self) -> String {
        PATH.of(&self.service)
    }

    #[zbus(property)]
    async fn state(&self) -> String {
        STATE.of(&self.service)
    }

    #[zbus(property)]
    async fn last_error(&self) -> String {
        LAST_ERROR.of(&self.service)
    }

    #[zbus(property)]
    async fn overall(&self) -> konedrive_dbus::overall::Overall {
        OVERALL.of(&self.service)
    }

    #[zbus(property)]
    async fn trouble(&self) -> String {
        TROUBLE.of(&self.service)
    }

    #[zbus(property)]
    async fn paused(&self) -> bool {
        PAUSED.of(&self.service)
    }

    #[zbus(property)]
    async fn paused_until(&self) -> i64 {
        PAUSED_UNTIL.of(&self.service)
    }

    #[zbus(property)]
    async fn held_back(&self) -> String {
        HELD_BACK.of(&self.service)
    }

    #[zbus(property)]
    async fn writable(&self) -> bool {
        WRITABLE.of(&self.service)
    }

    #[zbus(property)]
    async fn live_changes(&self) -> String {
        LIVE_CHANGES.of(&self.service)
    }

    #[zbus(property)]
    async fn items_listed(&self) -> u64 {
        ITEMS_LISTED.of(&self.service)
    }

    #[zbus(property)]
    async fn items_placed(&self) -> u64 {
        ITEMS_PLACED.of(&self.service)
    }

    #[zbus(property)]
    async fn skipped_count(&self) -> u64 {
        SKIPPED_COUNT.of(&self.service)
    }

    #[zbus(property)]
    async fn last_checked(&self) -> i64 {
        LAST_CHECKED.of(&self.service)
    }

    #[zbus(property)]
    async fn local_bytes(&self) -> u64 {
        LOCAL_BYTES.of(&self.service)
    }

    #[zbus(property)]
    async fn pinned_count(&self) -> u32 {
        PINNED_COUNT.of(&self.service)
    }

    // The properties a call sets and announces, and the folder's record.

    #[zbus(property)]
    async fn thumbnails(&self) -> bool {
        self.service.run_settings().thumbnails
    }

    #[zbus(property)]
    async fn source(&self) -> String {
        self.service.root_source()
    }

    #[zbus(property)]
    async fn ignore_patterns(&self) -> Vec<String> {
        self.service.ignore_patterns()
    }
}
