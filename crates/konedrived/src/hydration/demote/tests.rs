use std::os::unix::fs::{FileExt, MetadataExt};

use konedrive_fs::placeholder::{read_progress, write_progress, write_stamp};

use super::*;
use crate::hydration::testing::{content, placeholder, read_back};

/// A file of ours holding `data`, in `state`.
fn holding(dir: &std::path::Path, data: &[u8], state: State) -> File {
    let file = placeholder(dir, "file.bin", "ITEM", data.len() as u64);
    file.write_all_at(data, 0).unwrap();
    write_state(&file, state).unwrap();
    file
}

fn as_it_is(file: &File) -> Shape {
    Shape { size: None, times: FileTimes::of(file).unwrap() }
}

/// A file is emptied only in a state that says its content is not to be trusted, and
/// without a lease only in the one a fill wrote. A file that reads `hydrated` may carry an
/// ignore mark, and emptying it is the zeros case; one that reads `dehydrating` is a
/// free-up's, not a fill's.
#[test]
fn a_file_in_another_state_than_the_caller_vouches_for_is_left_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let data = content(8192, 1);
    for state in [State::Hydrated, State::OnlineOnly, State::Dehydrating] {
        let file = holding(dir.path(), &data, state);
        let demoted = demote(&file, Keep::Checkpoint, as_it_is(&file), Held::Fill).unwrap();
        assert_eq!(demoted, Demoted::Left(Some(state)));
        assert!(read_back(&file) == data, "{state:?}: the content must be left alone");
        assert_eq!(read_state(&file).unwrap(), Some(state), "and so must the state");
        std::fs::remove_file(dir.path().join("file.bin")).unwrap();
    }
    let file = holding(dir.path(), &data, State::Hydrated);
    let lease = WriteLease::take(&file).unwrap().expect("nobody else has the file open");
    let demoted = demote(&file, Keep::Nothing, as_it_is(&file), Held::Lease(&lease)).unwrap();
    assert_eq!(demoted, Demoted::Left(Some(State::Hydrated)));
    assert!(read_back(&file) == data);
}

/// The prefix a checkpoint counts is kept only of a download: a file found `dehydrating`
/// was whole, and a checkpoint left on it counts nothing.
#[test]
fn a_checkpoint_is_kept_of_an_interrupted_download_and_of_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let data = content(256 * 1024, 2);
    let checkpoint = Progress { ctag: "c1".into(), bytes: 64 * 1024 };
    for (state, kept) in [(State::Hydrating, 64 * 1024), (State::Dehydrating, 0)] {
        let file = holding(dir.path(), &data, state);
        write_stamp(&file).unwrap();
        write_progress(&file, &checkpoint).unwrap();
        let lease = WriteLease::take(&file).unwrap().expect("nobody else has the file open");

        let demoted = demote(&file, Keep::Checkpoint, as_it_is(&file), Held::Lease(&lease)).unwrap();

        assert_eq!(demoted, Demoted::Done { kept }, "{state:?}");
        assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
        assert_eq!(konedrive_fs::placeholder::read_stamp(&file).unwrap(), None);
        let back = read_back(&file);
        assert!(back[..kept as usize] == data[..kept as usize], "the prefix is kept");
        assert!(back[kept as usize..].iter().all(|b| *b == 0), "everything past it is punched");
        assert_eq!(read_progress(&file).unwrap(), (kept > 0).then(|| checkpoint.clone()));
        drop(lease);
        std::fs::remove_file(dir.path().join("file.bin")).unwrap();
    }
}

/// A download stopped because its item was removed leaves a placeholder: no content, no
/// checkpoint, the size it had.
#[test]
fn a_stopped_download_is_a_placeholder_again() {
    let dir = tempfile::tempdir().unwrap();
    let data = content(256 * 1024, 3);
    let file = holding(dir.path(), &data, State::Hydrating);
    write_progress(&file, &Progress { ctag: "c1".into(), bytes: 64 * 1024 }).unwrap();
    // As the materializer has it: open for reading.
    let read_only = File::open(dir.path().join("file.bin")).unwrap();

    back_to_placeholder(&read_only);

    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
    assert_eq!(read_progress(&file).unwrap(), None);
    assert_eq!(file.metadata().unwrap().len(), data.len() as u64);
    assert_eq!(file.metadata().unwrap().blocks(), 0, "nothing of the partial content is left");
}
