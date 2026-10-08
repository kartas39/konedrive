//! `konedrive-fuse mount <folder> <store> --local-dir <dir>` serves the store over the folder,
//! read-only, with content from a directory. `konedrive-fuse seed <store> <dir>` makes a store
//! that lists what `<dir>` holds, each file's id its path there: the tests' cloud.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use konedrive_graph::quickxor::QuickXor;
use konedrive_tree::{Change, Kind, NewTree, Placement, Row, TreeStore};
use konedrive_graph::drive::DriveClient;
use konedrive_graph::token::StaticToken;
use konedrived::hydration::graph_source::GraphSource;
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
        ["ctl", socket, verb, path] => (|| {
            use std::io::{BufRead, Write};
            let mut conn = std::os::unix::net::UnixStream::connect(socket)?;
            writeln!(conn, "{verb} {path}")?;
            let mut answer = String::new();
            std::io::BufReader::new(conn).read_line(&mut answer)?;
            println!("{}", answer.trim());
            if answer.starts_with("refused") { Err("refused".into()) } else { Ok(()) }
        })(),
        ["mount", folder, store, "--local-dir", dir] => (|| {
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            let source = Arc::new(LocalDir::new(PathBuf::from(dir)));
            let store = TreeStore::open(Path::new(store))?;
            Ok(konedrive_fuse::serve(Path::new(folder), store, source, runtime.handle().clone(), None, None)?)
        })(),
        // OneDrive itself, or what stands in for it: Graph's root and a file holding an access token.
        ["mount", folder, store, "--graph", base, "--token-file", token, rest @ ..] => (|| {
            let (every, socket) = match rest {
                ["--poll-secs", n, "--control", socket] => (n.parse()?, Some(Path::new(socket))),
                ["--poll-secs", n] => (n.parse()?, None),
                [] => (30, None),
                _ => return Err("after --token-file: --poll-secs <n> [--control <socket>]".into()),
            };
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            let token = std::fs::read_to_string(token)?.trim().to_owned();
            let _in_runtime = runtime.enter();
            let drive = DriveClient::new(url::Url::parse(base)?, Arc::new(StaticToken::new(token)))?;
            let source = Arc::new(GraphSource::new(drive.clone()));
            let store = TreeStore::open(Path::new(store))?;
            let follow = konedrive_fuse::Follow { drive, every: std::time::Duration::from_secs(every) };
            Ok(konedrive_fuse::serve(Path::new(folder), store, source, runtime.handle().clone(), Some(follow), socket)?)
        })(),
        _ => Err("usage: konedrive-fuse mount <folder> <store> (--local-dir <dir> | --graph <url> --token-file <file> [--poll-secs <n> [--control <socket>]]) | ctl <socket> pin|unpin|free-up <path> | seed <store> <dir>".into()),
    };
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("konedrive-fuse: {e}");
            ExitCode::FAILURE
        }
    }
}
