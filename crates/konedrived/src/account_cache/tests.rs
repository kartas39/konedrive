use super::*;

fn info() -> AccountInfo {
    AccountInfo {
        display_name: "Ann".into(),
        email: "ann@example.com".into(),
        quota_used: 1,
        quota_total: 2,
        quota_remaining: 1,
        quota_state: "normal".into(),
        quota_read_at: 3,
        fetched_at: 3,
        granted_scopes: "Files.Read User.Read".into(),
        drive_id: "D1".into(),
    }
}

#[test]
fn round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("state").join("account.json");
    save(&file, &info()).unwrap();
    assert_eq!(load(&file), Some(info()));
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
