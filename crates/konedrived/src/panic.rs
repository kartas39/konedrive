//! What a caught panic said, for the places that catch one and go on.

/// The message of `panic`, as `catch_unwind` or a task's `JoinError` hands it over.
pub fn message(panic: Box<dyn std::any::Any + Send>) -> String {
    match panic.downcast::<String>() {
        Ok(message) => *message,
        Err(panic) => panic.downcast::<&'static str>().map(|message| (*message).to_owned()).unwrap_or_else(|_| "no message".into()),
    }
}
