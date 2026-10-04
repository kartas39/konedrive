use super::*;
use std::sync::Arc;

#[test]
fn a_lock_whose_holder_panicked_is_taken_and_its_data_read() {
    let mutex = Arc::new(Mutex::new(1));
    let shared = Arc::new(RwLock::new(1));
    let (held, written) = (Arc::clone(&mutex), Arc::clone(&shared));
    let holder = std::thread::spawn(move || {
        let (mut held, mut written) = (lock(&held), write(&written));
        (*held, *written) = (2, 2);
        panic!("under both locks");
    });
    assert!(holder.join().is_err());
    assert!(mutex.is_poisoned() && shared.is_poisoned());

    assert_eq!(*lock(&mutex), 2);
    assert_eq!(*read(&shared), 2);
    *write(&shared) = 3;
    assert_eq!(*read(&shared), 3);
}
