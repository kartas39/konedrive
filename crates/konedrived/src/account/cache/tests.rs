use super::*;

fn info() -> AccountInfo {
    AccountInfo {
        display_name: "Ann".into(),
        email: "ann@example.com".into(),
        quota: QuotaFigures { used: 1, total: 2, remaining: 1, state: "normal".into(), read_at: 3 },
        fetched_at: 3,
        granted_scopes: "Files.Read User.Read".into(),
        drive_id: "D1".into(),
    }
}

#[test]
fn missing_or_corrupt_files_load_as_none() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("account.json");
    assert_eq!(load(&file), None);
    std::fs::write(&file, "{not json").unwrap();
    assert_eq!(load(&file), None);
}

#[test]
fn remove_tolerates_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("account.json");
    remove(&file);
    save(&file, &info()).unwrap();
    remove(&file);
    assert!(!file.exists());
}

/// What is saved loads again, in a directory made for it; and `account.json` as the
/// installed versions wrote it still loads, with its keys where they were: the newest one
/// whole, and one from before the remaining space, the state and the time of the read were
/// kept as a quota not read.
#[test]
fn a_saved_file_and_a_file_of_an_installed_version_load() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("state").join("account.json");
    save(&file, &info()).unwrap();
    assert_eq!(load(&file), Some(info()));
    std::fs::write(
        &file,
        r#"{"display_name": "Ann", "email": "ann@example.com", "quota_used": 1, "quota_total": 2, "quota_remaining": 1,
            "quota_state": "normal", "quota_read_at": 3, "fetched_at": 3, "granted_scopes": "Files.Read User.Read", "drive_id": "D1"}"#,
    )
    .unwrap();
    assert_eq!(load(&file), Some(info()));
    save(&file, &info()).unwrap();
    let written = std::fs::read_to_string(&file).unwrap();
    assert!(written.contains("\"quota_used\": 1") && written.contains("\"quota_read_at\": 3"), "{written}");

    std::fs::write(&file, r#"{"display_name": "Ann", "email": "", "quota_used": 1, "quota_total": 2, "fetched_at": 3}"#).unwrap();
    let older = load(&file).unwrap();
    assert_eq!(older.quota, QuotaFigures { used: 1, total: 2, ..QuotaFigures::default() });
    assert_eq!(older.quota.as_drive_quota().remaining, None, "not read");
}
