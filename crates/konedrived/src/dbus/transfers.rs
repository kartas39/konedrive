use konedrive_dbus::rows;
use zbus::interface;

use crate::dbus::properties::*;
use crate::dbus::Transfers;

/// Every property is a row of `properties`, which says what it is.
#[interface(name = "org.konedrive.Transfers")]
impl Transfers {
    #[zbus(property)]
    async fn downloads(&self) -> Vec<rows::Transfer> {
        DOWNLOADS.of(&self.service)
    }

    #[zbus(property)]
    async fn uploads(&self) -> Vec<rows::Transfer> {
        UPLOADS.of(&self.service)
    }

    #[zbus(property)]
    async fn download_speed(&self) -> u64 {
        DOWNLOAD_SPEED.of(&self.service)
    }

    #[zbus(property)]
    async fn upload_speed(&self) -> u64 {
        UPLOAD_SPEED.of(&self.service)
    }

    #[zbus(property)]
    async fn active_downloads(&self) -> u32 {
        ACTIVE_DOWNLOADS.of(&self.service)
    }

    #[zbus(property)]
    async fn active_uploads(&self) -> u32 {
        ACTIVE_UPLOADS.of(&self.service)
    }

    #[zbus(property)]
    async fn pool_in_use(&self) -> u32 {
        POOL_IN_USE.of(&self.service)
    }

    #[zbus(property)]
    async fn large_files(&self) -> u32 {
        LARGE_FILES.of(&self.service)
    }

    #[zbus(property)]
    async fn pool_size(&self) -> u32 {
        POOL_SIZE.of(&self.service)
    }

    #[zbus(property)]
    async fn pool_ceiling(&self) -> u32 {
        POOL_CEILING.of(&self.service)
    }

    #[zbus(property)]
    async fn large_streams(&self) -> u32 {
        LARGE_STREAMS.of(&self.service)
    }

    #[zbus(property)]
    async fn large_stream_limit(&self) -> u32 {
        LARGE_STREAM_LIMIT.of(&self.service)
    }

    #[zbus(property)]
    async fn retry_after(&self) -> u32 {
        RETRY_AFTER.of(&self.service)
    }

    #[zbus(property)]
    async fn download_left_count(&self) -> u32 {
        DOWNLOAD_LEFT_COUNT.of(&self.service)
    }

    #[zbus(property)]
    async fn download_left_bytes(&self) -> u64 {
        DOWNLOAD_LEFT_BYTES.of(&self.service)
    }

    #[zbus(property)]
    async fn download_done_bytes(&self) -> u64 {
        DOWNLOAD_DONE_BYTES.of(&self.service)
    }

    #[zbus(property)]
    async fn download_time_left(&self) -> u32 {
        DOWNLOAD_TIME_LEFT.of(&self.service)
    }

    #[zbus(property)]
    async fn upload_left_count(&self) -> u32 {
        UPLOAD_LEFT_COUNT.of(&self.service)
    }

    #[zbus(property)]
    async fn upload_left_bytes(&self) -> u64 {
        UPLOAD_LEFT_BYTES.of(&self.service)
    }

    #[zbus(property)]
    async fn upload_done_bytes(&self) -> u64 {
        UPLOAD_DONE_BYTES.of(&self.service)
    }

    #[zbus(property)]
    async fn upload_time_left(&self) -> u32 {
        UPLOAD_TIME_LEFT.of(&self.service)
    }
}
