use std::path::PathBuf;

use konedrive_tree::LocalItemId;

use super::ReadConnections;
use crate::fuse::item_fixture::ItemFixture;

/// The names the folder shows in its root, read through `connections` on this thread.
fn read_root_names(connections: &ReadConnections) -> Vec<String> {
    let mut names: Vec<String> = connections
        .read(|reader| reader.list_children(LocalItemId::ROOT))
        .unwrap()
        .into_iter()
        .filter_map(|item| item.local.and_then(|local| local.place))
        .map(|place| place.name.to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn each_thread_reads_through_a_connection_of_its_own() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_file("a.txt", b"a");
    let connections = &fixture.read_connections;
    assert_eq!(connections.count_connections(), 0, "none is opened before a thread asks");
    std::thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(|| assert_eq!(read_root_names(connections), ["a.txt"])).join().unwrap();
        }
    });
    assert_eq!(connections.count_connections(), 2);
    assert_eq!(read_root_names(connections), ["a.txt"]);
    assert_eq!(connections.count_connections(), 3, "this thread's too");
}

#[test]
fn a_thread_asks_twice_through_one_connection() {
    let fixture = ItemFixture::new();
    assert_eq!(read_root_names(&fixture.read_connections), Vec::<String>::new());
    assert_eq!(read_root_names(&fixture.read_connections), Vec::<String>::new());
    assert_eq!(fixture.read_connections.count_connections(), 1);
}

#[test]
fn a_reader_sees_what_the_writer_committed_before_the_question() {
    let fixture = ItemFixture::new();
    assert_eq!(read_root_names(&fixture.read_connections), Vec::<String>::new());
    fixture.show_onedrive_file("a.txt", b"a");
    assert_eq!(read_root_names(&fixture.read_connections), ["a.txt"], "the connection opened before the commit sees it");
}

#[test]
fn a_closure_reads_one_moment_though_the_writer_commits_while_it_runs() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_file("a.txt", b"a");
    let names = |reader: &konedrive_tree::ItemReader<'_>| reader.list_children(LocalItemId::ROOT).map(|items| items.len());
    let (before, after) = fixture
        .read_connections
        .read(|reader| {
            let before = names(reader)?;
            // The writer commits while the closure runs (a test may; a part must not).
            fixture.show_onedrive_file("b.txt", b"b");
            Ok((before, names(reader)?))
        })
        .unwrap();
    assert_eq!((before, after), (1, 1), "the second read is of the moment of the first");
    assert_eq!(read_root_names(&fixture.read_connections), ["a.txt", "b.txt"], "the next question sees the commit");
}

#[test]
fn a_store_that_cannot_be_opened_for_reading_is_an_error_of_the_question_not_a_panic() {
    let fixture = ItemFixture::new();
    let connections = ReadConnections::new(PathBuf::from(fixture.directory.path().join("no-such-directory/tree.db")));
    assert!(connections.read(|reader| reader.find_item(LocalItemId::ROOT)).is_err());
    assert_eq!(connections.count_connections(), 0, "nothing is kept of a connection that was not opened");
}

#[test]
fn after_the_close_a_question_is_an_error_and_no_connection_is_opened() {
    let fixture = ItemFixture::new();
    assert_eq!(read_root_names(&fixture.read_connections), Vec::<String>::new());
    fixture.read_connections.close();
    assert_eq!(fixture.read_connections.count_connections(), 0);
    assert!(fixture.read_connections.read(|reader| reader.find_item(LocalItemId::ROOT)).is_err());
    assert_eq!(fixture.read_connections.count_connections(), 0);
}
