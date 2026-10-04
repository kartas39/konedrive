//! What is done about a panic by the places that go on after one: the message of a caught
//! panic, and a lock taken whether or not a holder of it panicked.
//!
//! The daemon catches a panic and goes on, so a lock is never left unusable by one: every
//! lock of `std::sync` is taken through [`lock`], [`read`] or [`write`]
//! (`scripts/check-structure.sh` refuses the other spellings). Whoever takes it next finds
//! the data as the panic left it, so what is done under such a lock is kept to steps that
//! cannot leave it half-changed.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// The message of `panic`, as `catch_unwind` or a task's `JoinError` hands it over.
pub fn message(panic: Box<dyn std::any::Any + Send>) -> String {
    match panic.downcast::<String>() {
        Ok(message) => *message,
        Err(panic) => panic.downcast::<&'static str>().map(|message| (*message).to_owned()).unwrap_or_else(|_| "no message".into()),
    }
}

/// `mutex`, locked; also after a holder of it panicked.
pub fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `lock`, held for reading; also after a writer of it panicked.
pub fn read<T: ?Sized>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

/// `lock`, held for writing; also after a writer of it panicked.
pub fn write<T: ?Sized>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
