use super::*;
use crate::{
    ACCOUNTS_INTERFACE_NAME, ACCOUNTS_PATH, ACCOUNT_INTERFACE_NAME, ACTIVITY_LOG_INTERFACE_NAME,
    CONFLICTS_INTERFACE_NAME, FILES_INTERFACE_NAME, FOLDER_INTERFACE_NAME, LOCAL_SCAN_INTERFACE_NAME, SERVICE_NAME,
    TOKEN_EXPORT_INTERFACE_NAME, TRANSFERS_INTERFACE_NAME, UPLOAD_QUEUE_INTERFACE_NAME,
};
use zbus::proxy::Defaults;

/// The macro takes literals; this keeps them in step with the constants.
fn defaults<P: Defaults>() -> (String, String, Option<String>) {
    (
        P::INTERFACE.as_ref().expect("an interface").to_string(),
        P::DESTINATION.as_ref().expect("a destination").to_string(),
        P::PATH.as_ref().map(|path| path.to_string()),
    )
}

#[test]
fn proxies_use_the_published_names() {
    let manager = |interface: &str| {
        (
            interface.to_owned(),
            SERVICE_NAME.to_owned(),
            Some(ACCOUNTS_PATH.to_owned()),
        )
    };
    let account = |interface: &str| (interface.to_owned(), SERVICE_NAME.to_owned(), None);
    assert_eq!(defaults::<AccountsProxy>(), manager(ACCOUNTS_INTERFACE_NAME));
    assert_eq!(defaults::<FilesProxy>(), manager(FILES_INTERFACE_NAME));
    assert_eq!(defaults::<AccountProxy>(), account(ACCOUNT_INTERFACE_NAME));
    assert_eq!(defaults::<FolderProxy>(), account(FOLDER_INTERFACE_NAME));
    assert_eq!(defaults::<TransfersProxy>(), account(TRANSFERS_INTERFACE_NAME));
    assert_eq!(defaults::<UploadQueueProxy>(), account(UPLOAD_QUEUE_INTERFACE_NAME));
    assert_eq!(defaults::<ConflictsProxy>(), account(CONFLICTS_INTERFACE_NAME));
    assert_eq!(defaults::<LocalScanProxy>(), account(LOCAL_SCAN_INTERFACE_NAME));
    assert_eq!(defaults::<ActivityLogProxy>(), account(ACTIVITY_LOG_INTERFACE_NAME));
    assert_eq!(defaults::<TokenExportProxy>(), account(TOKEN_EXPORT_INTERFACE_NAME));
}
