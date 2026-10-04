use super::*;

/// The integer of a message and the helper's outcome are the same thing
/// both ways: 0 is success, and every other value is kept as it came.
#[test]
fn the_wire_integer_and_the_outcome_are_one_thing() {
    assert_eq!(Errno::from_wire(0), Ok(()));
    for errno in [libc::EPERM, libc::EIO, libc::ENOENT, -1, i32::MIN, i32::MAX, 256] {
        let outcome = Errno::from_wire(errno);
        assert_eq!(outcome, Err(Errno(errno)));
        assert_eq!(Errno::to_wire(&outcome), errno);
    }
    assert_eq!(Errno::to_wire(&Ok::<_, Errno>(())), 0);
}

/// An I/O error with no errno of its own is `EIO`; one with an errno keeps
/// it.
#[test]
fn an_io_error_gives_its_errno_or_eio() {
    assert_eq!(Errno::of(&io::Error::from_raw_os_error(libc::ENOENT)), libc::ENOENT);
    assert_eq!(Errno::of(&io::Error::other("no errno")), Errno::EIO);
}

/// What the kernel accepts in a denial is delivered as it is, and anything
/// else as `EIO`.
#[test]
fn only_what_the_kernel_accepts_is_delivered_as_it_is() {
    assert_eq!(Errno(libc::ENOSPC).deliverable(), libc::ENOSPC);
    assert_eq!(Errno::EAGAIN.deliverable(), Errno::EAGAIN);
    assert_eq!(Errno(libc::ENOENT).deliverable(), Errno::EIO);
    assert_eq!(Errno(-1).deliverable(), Errno::EIO);
}

/// The log shows the number.
#[test]
fn an_errno_is_shown_as_its_number() {
    assert_eq!(format!("{}", Errno::EIO), libc::EIO.to_string());
    assert_eq!(format!("{:?}", Errno::EPERM), "1");
}
