use std::sync::Arc;
use std::time::Duration;

use konedrive_graph::drive::RetryPolicy;
use konedrive_graph::token::StaticToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const NOW: i64 = 1_700_000_000;
const GIBIBYTE: u64 = 1 << 30;
const MEBIBYTE: u64 = 1 << 20;

fn quota(remaining: Option<u64>, state: &str) -> DriveQuota {
    DriveQuota { total: 10 * GIBIBYTE, used: 0, remaining, state: state.into() }
}

fn space() -> OneDriveSpace {
    OneDriveSpace::new(Quota::detached())
}

/// What the account's quota says is left, and when it was read.
fn read_account_quota(space: &OneDriveSpace) -> (u64, i64) {
    let figures = space.quota.state().get().quota;
    (figures.remaining, figures.read_at)
}

/// A drive whose quota answers `status` with `quota`, and is asked `times` times.
async fn drive(status: u16, quota: serde_json::Value, times: u64) -> (MockServer, DriveClient) {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/me/drive")).respond_with(ResponseTemplate::new(status).set_body_json(serde_json::json!({ "id": "drive", "quota": quota }))).expect(times).mount(&server).await;
    let retry = RetryPolicy { attempts: 1, default_wait: Duration::from_millis(1), max_wait: Duration::from_millis(1) };
    let drive = DriveClient::new(url::Url::parse(&format!("{}/", server.uri())).unwrap(), Arc::new(StaticToken::new("T"))).unwrap().with_retry(retry);
    (server, drive)
}

#[test]
fn a_quota_that_says_exceeded_or_less_than_a_mebibyte_left_is_full() {
    assert_eq!(decide_is_full(&quota(Some(5 * GIBIBYTE), "exceeded")), Some(true));
    assert_eq!(decide_is_full(&quota(None, "exceeded")), Some(true));
    assert_eq!(decide_is_full(&quota(Some(MEBIBYTE - 1), "critical")), Some(true));
    assert_eq!(decide_is_full(&quota(Some(0), "")), Some(true));
    assert_eq!(decide_is_full(&quota(Some(MEBIBYTE), "critical")), Some(false));
    assert_eq!(decide_is_full(&quota(None, "normal")), Some(false));
    assert_eq!(decide_is_full(&quota(None, "")), None, "no remaining and no state say nothing");
}

#[test]
fn a_quota_that_says_nothing_of_the_space_changes_nothing() {
    let space = space();
    assert!(!space.is_full() && space.find_next_read().is_none(), "not full, nothing read yet");
    assert_eq!(space.apply_quota(&quota(None, ""), NOW), SpaceChange::NotKnown);
    assert!(!space.is_full() && space.find_next_read().is_none());
    assert_eq!(read_account_quota(&space), (0, 0), "nothing of it is written to the account's quota");
    // Nor when OneDrive is full.
    assert_eq!(space.apply_quota(&quota(Some(0), "exceeded"), NOW), SpaceChange::BecameFull);
    assert_eq!(space.apply_quota(&quota(None, ""), NOW + 60), SpaceChange::NotKnown);
    assert!(space.is_full());
    assert_eq!(space.find_next_read(), Some(NOW + 30 * 60));
}

#[test]
fn a_quota_read_with_space_again_ends_full_and_says_how_much_is_free() {
    let space = space();
    assert_eq!(space.apply_quota(&quota(Some(MEBIBYTE - 1), "critical"), NOW), SpaceChange::BecameFull);
    assert!(space.is_full());
    assert_eq!(space.find_next_read(), Some(NOW + 30 * 60), "read again every 30 minutes while full");
    assert_eq!(read_account_quota(&space), (MEBIBYTE - 1, NOW), "the read is the account's quota's too");
    assert_eq!(space.apply_quota(&quota(Some(0), "exceeded"), NOW + 60), SpaceChange::StillFull);
    assert_eq!(space.find_next_read(), Some(NOW + 60 + 30 * 60));
    assert_eq!(space.apply_quota(&quota(Some(3 * GIBIBYTE), "normal"), NOW + 120), SpaceChange::HasSpace { free_space: 3 * GIBIBYTE, was_full: true });
    assert!(!space.is_full());
    // A file may still wait as too big for what is free: the next read stays set.
    assert_eq!(space.find_next_read(), Some(NOW + 120 + 30 * 60));
    assert_eq!(space.apply_quota(&quota(Some(4 * GIBIBYTE), "normal"), NOW + 180), SpaceChange::HasSpace { free_space: 4 * GIBIBYTE, was_full: false });
    // A drive that was never full and refused nothing is not read by itself.
    let space = self::space();
    assert_eq!(space.apply_quota(&quota(Some(GIBIBYTE), "normal"), NOW), SpaceChange::HasSpace { free_space: GIBIBYTE, was_full: false });
    assert_eq!(space.find_next_read(), None);
}

#[tokio::test]
async fn a_507_with_space_left_is_the_files_own_refusal_and_not_a_full_onedrive() {
    let (_server, drive) = drive(200, serde_json::json!({ "total": 10 * GIBIBYTE, "used": 7 * GIBIBYTE, "remaining": 3 * GIBIBYTE, "state": "normal" }), 1).await;
    let space = space();
    assert_eq!(space.refuse_for_space(&drive, 5 * GIBIBYTE, NOW).await, StepRefusal::NoSpace { free_space: Some(3 * GIBIBYTE) });
    assert!(!space.is_full(), "the file alone is too big");
    assert_eq!(space.find_next_read(), Some(NOW + 30 * 60), "read again while a file waits as too big");
    assert_eq!(read_account_quota(&space), (3 * GIBIBYTE, NOW));
}

#[tokio::test]
async fn a_507_with_no_space_left_makes_onedrive_full() {
    let (_server, drive) = drive(200, serde_json::json!({ "total": 10 * GIBIBYTE, "used": 10 * GIBIBYTE, "remaining": 0, "state": "exceeded" }), 1).await;
    let space = space();
    assert_eq!(space.refuse_for_space(&drive, 1000, NOW).await, StepRefusal::NoSpace { free_space: None });
    assert!(space.is_full());
    assert_eq!(space.find_next_read(), Some(NOW + 30 * 60));
}

#[tokio::test]
async fn a_507_whose_quota_cannot_be_read_counts_as_a_full_onedrive() {
    let (_server, drive) = drive(404, serde_json::json!({}), 1).await;
    let space = space();
    assert_eq!(space.refuse_for_space(&drive, 1000, NOW).await, StepRefusal::NoSpace { free_space: None }, "OneDrive just said so");
    assert!(space.is_full());
    assert_eq!(space.find_next_read(), Some(NOW + 30 * 60), "and the next read decides");
    // A quota that says nothing of the space is none either.
    let (_server, drive) = self::drive(200, serde_json::json!({ "total": 10 * GIBIBYTE }), 1).await;
    let space = self::space();
    assert_eq!(space.refuse_for_space(&drive, 1000, NOW).await, StepRefusal::NoSpace { free_space: None });
    assert!(space.is_full());
}

/// Refusals of steps that run together cost one request.
#[tokio::test]
async fn a_quota_read_less_than_ten_seconds_ago_answers_another_507() {
    let (_server, drive) = drive(200, serde_json::json!({ "total": 10 * GIBIBYTE, "used": 7 * GIBIBYTE, "remaining": 3 * GIBIBYTE, "state": "normal" }), 1).await;
    let space = space();
    let now = crate::clock::unix_now();
    assert_eq!(space.refuse_for_space(&drive, 5 * GIBIBYTE, now).await, StepRefusal::NoSpace { free_space: Some(3 * GIBIBYTE) });
    assert_eq!(space.refuse_for_space(&drive, 4 * GIBIBYTE, now).await, StepRefusal::NoSpace { free_space: Some(3 * GIBIBYTE) });
    // The account's own read of a moment ago is used likewise.
    let (_server, unasked) = self::drive(200, serde_json::json!({}), 0).await;
    let space = self::space();
    space.quota.read_at(&quota(Some(0), "exceeded"), now);
    assert_eq!(space.refuse_for_space(&unasked, 1000, now).await, StepRefusal::NoSpace { free_space: None });
    assert!(space.is_full());
}

#[test]
fn what_is_free_is_what_the_last_quota_applied_said_less_what_went_up_since() {
    let space = space();
    assert_eq!(space.find_free_space(), None, "before any quota was applied");
    // The account's own figures are not this folder's to go by before it applied a quota.
    space.quota.read_at(&quota(Some(7 * GIBIBYTE), "normal"), NOW);
    assert_eq!(space.find_free_space(), None);
    assert_eq!(space.apply_quota(&quota(None, ""), NOW), SpaceChange::NotKnown);
    assert_eq!(space.find_free_space(), None, "a quota that says nothing of the space is none applied");

    assert_eq!(space.apply_quota(&quota(Some(5 * GIBIBYTE), "normal"), NOW), SpaceChange::HasSpace { free_space: 5 * GIBIBYTE, was_full: false });
    assert_eq!(space.find_free_space(), Some(5 * GIBIBYTE));
    space.count_uploaded(GIBIBYTE);
    assert_eq!(space.find_free_space(), Some(4 * GIBIBYTE), "less what was uploaded since");

    // Full: none, whatever the figure; with space again, that read's.
    assert_eq!(space.apply_quota(&quota(Some(3 * GIBIBYTE), "exceeded"), NOW + 60), SpaceChange::BecameFull);
    assert_eq!(space.find_free_space(), None);
    assert_eq!(space.apply_quota(&quota(Some(2 * GIBIBYTE), "normal"), NOW + 120), SpaceChange::HasSpace { free_space: 2 * GIBIBYTE, was_full: true });
    assert_eq!(space.find_free_space(), Some(2 * GIBIBYTE));
}

/// A read with a state and no `remaining` after one that said 5 GiB.
#[test]
fn a_quota_with_a_state_and_no_remaining_leaves_nothing_free_to_fit_into() {
    let space = space();
    space.apply_quota(&quota(Some(5 * GIBIBYTE), "normal"), NOW);
    assert_eq!(space.apply_quota(&quota(None, "nearing"), NOW + 60), SpaceChange::HasSpace { free_space: 0, was_full: false });
    assert_eq!(read_account_quota(&space), (5 * GIBIBYTE, NOW + 60), "the account's figure stays what the read before said");
    assert_eq!(space.find_free_space(), Some(0), "and is not what this read said: nothing that waits is taken to fit");
    // The next read that says what is left is gone by again.
    space.apply_quota(&quota(Some(GIBIBYTE), "nearing"), NOW + 120);
    assert_eq!(space.find_free_space(), Some(GIBIBYTE));
}

#[tokio::test]
async fn what_is_free_after_a_507_is_what_the_read_it_made_said() {
    let (_server, drive) = drive(200, serde_json::json!({ "total": 10 * GIBIBYTE, "used": 0, "remaining": 2 * GIBIBYTE, "state": "normal" }), 1).await;
    let space = space();
    assert_eq!(space.refuse_for_space(&drive, 3 * GIBIBYTE, NOW).await, StepRefusal::NoSpace { free_space: Some(2 * GIBIBYTE) });
    assert_eq!(space.find_free_space(), Some(2 * GIBIBYTE), "the file alone was too big");
    // A 507 with a quota that cannot be read makes OneDrive full: nothing is free.
    let (_server, drive) = self::drive(404, serde_json::json!({}), 1).await;
    let space = self::space();
    assert_eq!(space.refuse_for_space(&drive, 3 * GIBIBYTE, NOW).await, StepRefusal::NoSpace { free_space: None });
    assert_eq!(space.find_free_space(), None);
}

#[test]
fn a_refresh_reads_the_quota_and_a_due_read_waits_after_one_that_failed() {
    let (retry_wait, now) = (60, 1_000);
    assert!(!decide_is_quota_read(false, None, None, retry_wait, now), "never full: nothing is due");
    assert!(decide_is_quota_read(true, None, None, retry_wait, now), "a refresh reads it");
    assert!(decide_is_quota_read(true, Some(now + 500), Some(now - 1), retry_wait, now), "a refresh reads it whatever was read before");
    assert!(!decide_is_quota_read(false, Some(now + 1), None, retry_wait, now), "the next read is still to come");
    assert!(decide_is_quota_read(false, Some(now), None, retry_wait, now), "the next read is due");
    assert!(!decide_is_quota_read(false, Some(now - 10), Some(now - 59), retry_wait, now), "a due read that failed is not asked again at once");
    assert!(decide_is_quota_read(false, Some(now - 10), Some(now - 60), retry_wait, now));
}
