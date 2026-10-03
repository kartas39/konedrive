use super::*;

/// And the governing property of this whole component: an
/// application must never read zeros where real content should be.
///
/// `fanotify(7)` allows every outstanding permission event when the
/// group's descriptor closes, so the helper exiting is **silent data
/// loss**, while the helper denying is an errno the application sees. That
/// asymmetry is what makes running out of descriptors — a resource
/// problem, and a self-correcting one, since the descriptors are held by
/// hydrations that are all going to finish — something to survive rather
/// than something to die of.
#[test]
fn running_out_of_descriptors_does_not_end_the_process() {
    assert_eq!(
        classify_read_failure(Errno::EMFILE),
        ReadFailure::Exhausted,
        "this process being out of descriptors must not close the fanotify group"
    );
    assert_eq!(
        classify_read_failure(Errno::ENFILE),
        ReadFailure::Exhausted,
        "nor must the machine being out of them"
    );
}

/// The other arms, so that "survivable" did not quietly become
/// "everything is survivable": an errno the group fd itself reports still
/// ends the loop, because a group that cannot be read cannot be answered.
#[test]
fn the_event_loops_other_read_failures_keep_their_meaning() {
    assert_eq!(classify_read_failure(Errno::EAGAIN), ReadFailure::Drained);
    assert_eq!(classify_read_failure(Errno::EINTR), ReadFailure::Interrupted);
    for errno in [Errno::EBADF, Errno::EINVAL, Errno::EFAULT] {
        assert_eq!(classify_read_failure(errno), ReadFailure::Fatal, "{errno} is not handled");
    }
}

/// The kernel opens each event's
/// descriptor with the group's `O_RDWR` against the **opener's** mount,
/// and when that open fails `read()` of the group returns its errno for
/// that one event — which the kernel has already denied. Measured:
/// `EROFS` for an open through a read-only mount (a Flatpak app with
/// `home:ro`), `ETXTBSY` for a second open of a running executable. As
/// `Fatal`, either one ended the helper, and every suspended open was
/// then allowed onto its unfilled placeholder: 65 536 zero bytes. Any
/// errno but the three the group descriptor itself can report is one
/// event's, and the loop goes on reading.
#[test]
fn an_event_the_kernel_could_not_hand_over_does_not_end_the_helper() {
    for errno in [
        Errno::EROFS,
        Errno::ETXTBSY,
        Errno::EACCES,
        Errno::EPERM,
        Errno::EIO,
        Errno::ENOMEM,
        Errno::ENXIO,
        Errno::ENODEV,
        Errno::EOVERFLOW,
        Errno::ESTALE,
    ] {
        assert_ne!(
            classify_read_failure(errno),
            ReadFailure::Fatal,
            "{errno}: one event the kernel could not hand over must not close the group — \
             closing it allows every suspended open onto its placeholder"
        );
    }
}
