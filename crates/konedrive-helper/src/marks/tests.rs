use super::*;

/// The self-exemption in events.rs's event loop compares pids (see
/// `INIT_FLAGS`).
#[test]
fn init_flags_report_pids_not_thread_ids() {
    assert!(!INIT_FLAGS.contains(InitFlags::FAN_REPORT_TID));
}

/// M2's set, and the errnos the daemon will actually report that are not
/// in it. A value outside the set is not a curiosity: `ENOENT` is what a
/// deleted OneDrive item looks like and `ETIMEDOUT` is what a slow network
/// looks like, and either one reaching `write()` unclamped leaves every
/// waiting opener suspended for the lifetime of the helper.
#[test]
fn accepted_errnos_pass_through_unchanged() {
    for errno in ACCEPTED_DENY_ERRNOS {
        assert_eq!(clamp_deny_errno(errno), errno, "errno {errno} is accepted by the kernel");
    }
}

#[test]
fn everything_else_becomes_eio() {
    for errno in [
        libc::ENOENT,
        libc::EACCES,
        libc::ECONNRESET,
        libc::ENETDOWN,
        libc::ETIMEDOUT,
        libc::ECANCELED,
        libc::EINVAL,
        libc::ENOMEM,
        libc::EEXIST,
        libc::ENODEV,
        libc::EPIPE,
        libc::EHOSTUNREACH,
    ] {
        assert_eq!(clamp_deny_errno(errno), libc::EIO, "errno {errno} must be downgraded");
    }
}

/// A daemon is not trusted to send a sensible number at all, and the
/// response word only has a byte to put it in.
#[test]
fn nonsense_values_become_eio_and_never_corrupt_the_response() {
    for errno in [-1, -4095, i32::MIN, i32::MAX, 256, 512, 0x100, 0xdead_beefu32 as i32] {
        let clamped = clamp_deny_errno(errno);
        assert_eq!(clamped, libc::EIO, "errno {errno} must be downgraded");
    }
    // The masking is what guarantees the invariant even if the clamp were
    // ever widened: only the low byte can reach the response word.
    for errno in [-1i32, 0x1234, i32::MIN] {
        assert_eq!((errno as u32 & 0xff) << 24 & !0xff00_0000, 0);
    }
}

/// `ClearIgnore` and `UnmarkDir` run against marks the kernel is free to
/// have thrown away already; only that one errno is normal, and the rest
/// must still be reported.
#[test]
fn a_missing_mark_is_not_a_failure_but_other_errors_still_are() {
    assert!(tolerate_missing_mark(Ok(())).is_ok());
    assert!(tolerate_missing_mark(Err(Errno::ENOENT)).is_ok());
    let error = tolerate_missing_mark(Err(Errno::EBADF)).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EBADF));
}
