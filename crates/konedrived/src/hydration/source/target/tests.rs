use std::sync::mpsc;
use std::time::Duration;

use super::*;

fn a_file() -> (tempfile::TempDir, File) {
    let dir = tempfile::tempdir().unwrap();
    let file = File::create(dir.path().join("file.bin")).unwrap();
    (dir, file)
}

/// A section runs on a blocking thread, never on the thread that drives the fill.
#[tokio::test]
async fn a_section_runs_off_the_thread_that_asked_for_it() {
    let (_dir, file) = a_file();
    let target = Target::new(file);
    let here = std::thread::current().id();
    assert_ne!(target.alone(|_| std::thread::current().id()).await, here);
    assert_ne!(target.beside(|_| std::thread::current().id()).await, here);
}

/// A section that runs alone starts when the sections running beside each other are over.
#[tokio::test]
async fn a_section_alone_waits_for_the_sections_under_way() {
    let (_dir, file) = a_file();
    let target = Target::new(file);
    let (started, has_started) = mpsc::channel();
    let (end, may_end) = mpsc::channel::<()>();
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (first, second) = (Arc::clone(&order), Arc::clone(&order));
    let beside = target.beside(move |_| {
        started.send(()).unwrap();
        may_end.recv().unwrap();
        first.lock().unwrap().push("beside");
    });
    let alone = async {
        tokio::task::spawn_blocking(move || has_started.recv().unwrap()).await.unwrap();
        let alone = target.alone(move |_| second.lock().unwrap().push("alone"));
        tokio::pin!(alone);
        // Asked for, and not started while the other runs.
        assert!(tokio::time::timeout(Duration::from_millis(50), &mut alone).await.is_err());
        end.send(()).unwrap();
        alone.await;
    };
    tokio::join!(beside, alone);
    assert_eq!(*order.lock().unwrap(), ["beside", "alone"]);
}
