//! `konedrive-fuse mount <folder> <store> --local-dir <dir>` serves the store over the folder,
//! read-only, with content from a directory. `konedrive-fuse seed <store> <dir>` makes a store
//! that lists what `<dir>` holds, each file's id its path there: the tests' cloud.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use konedrive_graph::quickxor::QuickXor;
use konedrive_tree::{Change, Kind, NewTree, Placement, Row, TreeStore};
use konedrived::hydration::source::LocalDir;

fn row(id: &str, parent: Option<&str>, name: &str, kind: Kind, size: u64, mtime: i64, quickxor: Option<String>) -> Row {
    Row {
        id: id.into(),
        parent_id: parent.map(Into::into),
        name: name.into(),
        kind,
        size,
        mtime,
        etag: None,
        ctag: Some(format!("c-{id}")),
        quickxor,
        mime: None,
        placement: Placement::Placed,
    }
}

fn seed_dir(dir: &Path, base: &Path, parent: &str, changes: &mut Vec<Change>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let (path, meta) = (entry.path(), entry.metadata()?);
        let id = path.strip_prefix(base).unwrap().to_string_lossy().into_owned();
        let name = entry.file_name().to_string_lossy().into_owned();
        let mtime = std::os::unix::fs::MetadataExt::mtime(&meta);
        if meta.is_dir() {
            changes.push(Change::Upsert(row(&id, Some(parent), &name, Kind::Folder, 0, mtime, None)));
            seed_dir(&path, base, &id, changes)?;
        } else {
            let mut hash = QuickXor::new();
            hash.update(&std::fs::read(&path)?);
            changes.push(Change::Upsert(row(&id, Some(parent), &name, Kind::File, meta.len(), mtime, Some(hash.finish_base64()))));
        }
    }
    Ok(())
}

fn seed(store: &Path, dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut changes = vec![Change::Root(row("root", None, "", Kind::Folder, 0, 0, None))];
    seed_dir(dir, dir, "root", &mut changes)?;
    let mut tree = TreeStore::open(store)?;
    tree.begin_staging(NewTree::Whole)?;
    tree.stage(&changes)?;
    tree.commit_staging("seeded")?;
    println!("seeded {} items", changes.len());
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let done: Result<(), Box<dyn std::error::Error>> = match args.as_slice() {
        ["seed", store, dir] => seed(Path::new(store), Path::new(dir)),
        ["mount", folder, store, "--local-dir", dir] => {
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            let source = Arc::new(LocalDir::new(PathBuf::from(dir)));
            konedrive_fuse::mount(Path::new(folder), Path::new(store), source, runtime.handle().clone()).map_err(Into::into)
        }
        _ => Err("usage: konedrive-fuse mount <folder> <store> --local-dir <dir> | seed <store> <dir>".into()),
    };
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("konedrive-fuse: {e}");
            ExitCode::FAILURE
        }
    }
}
