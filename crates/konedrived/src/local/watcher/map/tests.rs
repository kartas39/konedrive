use std::path::Path;

use konedrive_fs::handle::FileHandle;

use super::*;

fn fid(n: u8) -> Fid {
    Fid { fsid: [1, 2], handle: FileHandle { kind: 1, bytes: vec![n; 8] } }
}

#[test]
fn paths_follow_renames_and_a_subtree_goes_whole() {
    let mut map = DirMap::new(fid(0), true);
    map.place(fid(1), &fid(0), OsStr::new("a"), true).unwrap();
    map.place(fid(2), &fid(1), OsStr::new("b"), true).unwrap();
    map.place(fid(3), &fid(2), OsStr::new("c"), false).unwrap();
    assert_eq!(map.path(&fid(3)).unwrap(), Path::new("a/b/c"));
    assert_eq!(map.path(&fid(0)).unwrap(), Path::new(""));
    map.place(fid(1), &fid(0), OsStr::new("z"), false).unwrap();
    assert_eq!(map.path(&fid(3)).unwrap(), Path::new("z/b/c"), "a rename moves everything below it");
    assert_eq!(map.child(&fid(0), OsStr::new("a")), None, "not under its old name");
    assert!(map.get(&fid(1)).unwrap().marked, "a moved directory keeps its mark");
    assert_eq!(map.place(fid(1), &fid(3), OsStr::new("x"), true), Err(Stale), "not under itself");
    assert_eq!(map.place(fid(9), &fid(8), OsStr::new("x"), true), Err(Stale), "not under a stranger");
    assert_eq!(map.child(&fid(0), OsStr::new("z")), Some(fid(1)));
    map.place(fid(4), &fid(0), OsStr::new("y"), true).unwrap();
    assert_eq!(map.place(fid(5), &fid(0), OsStr::new("y"), true).unwrap(), vec![fid(4)], "one directory per name");
    assert_eq!(map.subtree(&fid(1)).len(), 3);
    let mut gone = map.remove(&fid(2));
    gone.sort_by_key(|f| f.handle.bytes[0]);
    assert_eq!(gone, vec![fid(2), fid(3)]);
    assert_eq!(map.path(&fid(3)), None);
    assert_eq!(map.len(), 3, "the root, z and y");
    assert!(map.remove(&fid(0)).is_empty(), "the root stays");
}
