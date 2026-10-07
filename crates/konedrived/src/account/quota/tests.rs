use std::sync::Mutex;

use super::*;

fn quota(total: u64, used: u64, remaining: Option<u64>, state: &str) -> DriveQuota {
    DriveQuota { total, used, remaining, state: state.into() }
}

/// A read replaces what it gives and keeps the rest; an upload comes off what is left
/// and onto what is used, but only once a read has said what is left.
#[test]
fn a_read_replaces_what_it_gives_and_uploads_move_the_figures() {
    let kept: Arc<Mutex<Vec<QuotaFigures>>> = Arc::default();
    let log = Arc::clone(&kept);
    let q = Quota::new(
        StateHandle::new(AccountSnapshot::default()),
        Some(Arc::new(move |s: &AccountSnapshot| {
            log.lock().unwrap().push(s.quota.clone())
        })),
    );
    q.uploaded(10);
    assert_eq!(q.state().get().quota, QuotaFigures::default(), "nothing read yet");

    q.read(&quota(100, 40, Some(60), "normal"));
    q.uploaded(10);
    let s = q.state().get().quota;
    assert_eq!((s.used, s.total, s.remaining, s.state.as_str()), (50, 100, 50, "normal"));

    q.read(&quota(0, 0, None, "nearing"));
    let s = q.state().get().quota;
    assert_eq!((s.used, s.total, s.remaining, s.state.as_str()), (50, 100, 50, "nearing"));
    assert_eq!(q.read_within(10), Some(quota(100, 50, Some(50), "nearing")));
    assert_eq!(kept.lock().unwrap().len(), 2, "kept at each read, not at each upload");
}
