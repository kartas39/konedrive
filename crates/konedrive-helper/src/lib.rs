//! The parts of the privileged helper that are worth exercising on their own.
//!
//! The binary is in `main.rs`; this library exists so that the fanotify
//! wrapper, the job table and the root registry can be linked by something
//! other than the daemon — specifically by the privileged VM checks in
//! `tests/vm`, which must call the code that actually ships rather than a
//! re-implementation of it. The ignore mark is the reason: the syscall
//! returns 0 whether or not it created anything (M3), so the only way to know
//! that the shipped `Marks::ignore_file` works is to run *it* and look at
//! `/proc/self/fdinfo/<group>`.

pub mod jobs;
pub mod marks;
pub mod outbox;
pub mod roots;
