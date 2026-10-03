use super::*;

#[test]
fn names_gather_per_directory_and_a_whole_directory_wins() {
    let mut batch = Batch::new();
    assert!(batch.is_empty());
    batch.name(Path::new("d"), OsStr::new("a"));
    batch.name(Path::new("d/e"), OsStr::new("."));
    batch.name(Path::new(""), OsStr::new("."));
    assert_eq!(batch.dirs[Path::new("d")], DirScope::Names(["a", "e"].into_iter().map(OsString::from).collect()));
    assert_eq!(batch.dirs.len(), 1, "the root's own event is not a change");
    let mut other = Batch::new();
    other.dir(Path::new("d"));
    batch.merge(other);
    batch.name(Path::new("d"), OsStr::new("b"));
    assert_eq!(batch.dirs[Path::new("d")], DirScope::Whole);
}
