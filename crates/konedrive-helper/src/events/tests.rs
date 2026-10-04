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

/// A panic over one event of a batch costs that event and the ones read
/// with it and not yet handled, which are each given up by name; the ones
/// before it were handled, and nothing unwinds into the loop.
#[test]
fn a_panic_over_one_event_gives_up_the_rest_of_its_batch_and_no_more() {
    let mut handled = Vec::new();
    let mut abandoned = Vec::new();
    let whole = contain_batch(
        1..=5,
        |n| {
            if n == 3 {
                panic!("deliberate: the third event of the batch");
            }
            handled.push(n);
        },
        |n| abandoned.push(n),
    );
    assert!(!whole);
    assert_eq!(handled, [1, 2]);
    assert_eq!(abandoned, [4, 5], "the third is dropped by the unwind, which denies it");
}

/// A batch with no panic in it is handled whole and nothing is given up.
#[test]
fn a_batch_without_a_panic_is_handled_whole() {
    let mut handled = Vec::new();
    let whole = contain_batch(1..=3, |n| handled.push(n), |n: i32| panic!("gave up {n}"));
    assert!(whole);
    assert_eq!(handled, [1, 2, 3]);
}

/// Giving one event up cannot stop the others from being given up.
#[test]
fn a_panic_while_giving_an_event_up_does_not_spare_the_ones_after_it() {
    let mut abandoned = Vec::new();
    let whole = contain_batch(
        1..=4,
        |_| panic!("deliberate: the first event of the batch"),
        |n| {
            if n == 2 {
                panic!("deliberate: while giving the second up");
            }
            abandoned.push(n);
        },
    );
    assert!(!whole);
    assert_eq!(abandoned, [3, 4]);
}
