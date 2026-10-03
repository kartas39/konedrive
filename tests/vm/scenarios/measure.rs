use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use konedrive_fs::placeholder::create_placeholder;
use konedrive_proto::SOCKET_PATH;
use konedrived::helper::HelperLink;
use konedrived::hydration::source::ContentSource;
use konedrived::hydration::server::serve_hydrations;
use konedrived::folder::locks::InodeLocks;

use crate::harness::{FAN_OPEN_PERM, HelperProc, Reader, TestSource, helper_marks};
use crate::ROOTS_FILE;

// ---------------------------------------------------------------------------
// measurement mode
// ---------------------------------------------------------------------------

/// `/proc/slabinfo`, summed as `active_objs × objsize` over every cache, which
/// is what §8's figures are deltas of.
fn slab_total() -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/slabinfo") else { return 0 };
    let mut total = 0u64;
    for line in text.lines().skip(2) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        let objs: u64 = fields[1].parse().unwrap_or(0);
        let size: u64 = fields[3].parse().unwrap_or(0);
        total += objs * size;
    }
    total
}

fn settled(label: &str) -> u64 {
    // SAFETY: a plain sync(2).
    unsafe { libc::sync() };
    let _ = std::fs::write("/proc/sys/vm/drop_caches", b"3");
    std::thread::sleep(Duration::from_millis(500));
    let total = slab_total();
    println!("  slab after {label}: {total} B");
    total
}

pub(crate) fn measure_mode(helper_binary: &Path, dirs: usize, files: usize) -> i32 {
    println!("== measurement: {dirs} directories, {files} files, on btrfs ==");
    let mount = PathBuf::from("/mnt/btrfs");
    let base = mount.join("measure");
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    let source_dir = base.join("source");
    for dir in [&root, &source_dir] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            println!("MEASURE-FAIL cannot create {dir:?}: {e}");
            return 1;
        }
    }

    // The tree, built before the helper knows anything about it, so the
    // startup walk is the thing being timed.
    let started = Instant::now();
    let per_dir = files.div_ceil(dirs.max(1));
    for d in 0..dirs {
        let dir = root.join(format!("d{:05}", d));
        if std::fs::create_dir_all(&dir).is_err() {
            println!("MEASURE-FAIL cannot create {dir:?}");
            return 1;
        }
        let handle = match File::open(&dir) {
            Ok(handle) => handle,
            Err(e) => {
                println!("MEASURE-FAIL cannot open {dir:?}: {e}");
                return 1;
            }
        };
        for f in 0..per_dir {
            let _ = create_placeholder(
                &handle,
                &format!("f{f:04}"),
                &format!("I{d}-{f}"),
                4096,
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            );
        }
    }
    println!("  built the tree in {:?}", started.elapsed());

    let _ = std::fs::remove_file(ROOTS_FILE);
    let _ = std::fs::remove_file(SOCKET_PATH);
    let log = PathBuf::from("/run/konedrive-helper-measure.log");
    let _ = std::fs::remove_file(&log);

    let before_marks = settled("the tree was built, before any mark");
    let mut helper = match HelperProc::start(helper_binary, &log) {
        Ok(helper) => helper,
        Err(e) => {
            println!("MEASURE-FAIL {e}");
            return 1;
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let source = TestSource::new(source_dir.clone());
    let (link, requests) = match runtime.block_on(HelperLink::connect(Path::new(SOCKET_PATH))) {
        Ok(pair) => pair,
        Err(e) => {
            println!("MEASURE-FAIL cannot connect: {e}");
            helper.stop();
            return 1;
        }
    };
    let locks = InodeLocks::new();
    runtime.spawn(serve_hydrations(
        link.clone(),
        requests,
        Arc::clone(&source) as Arc<dyn ContentSource>,
        locks,
    ));

    // Registration performs the whole `openat2` walk inside the helper, which
    // is the startup walk under another name.
    // Through `HelperLink` directly, as the DT_UNKNOWN scenario does:
    // `root::register_root` refuses a folder that is not empty,
    // and a tree that already exists is the whole point of timing the walk.
    let root_id = "measure-root";
    let root_handle = match File::open(&root) {
        Ok(handle) => handle,
        Err(e) => {
            println!("MEASURE-FAIL cannot open the root: {e}");
            helper.stop();
            return 1;
        }
    };
    let walk_started = Instant::now();
    if let Err(e) = runtime.block_on(link.register_root(&root_handle, root_id)) {
        println!("MEASURE-FAIL cannot register: {e}");
        helper.stop();
        return 1;
    }
    let walk = walk_started.elapsed();
    println!("  the walk of {dirs} directories took {walk:?}");
    let marks = helper_marks(helper.pid()).len();
    println!("  the group holds {marks} mark(s)");

    let after_marks = settled("every directory is marked");
    let per_mark = (after_marks.saturating_sub(before_marks)) as f64 / marks.max(1) as f64;
    println!("  per directory mark, after drop_caches: {per_mark:.1} B");

    // A first open, a second (ignored) open, and an open outside the root.
    let exe = std::env::current_exe().unwrap();
    let payload = vec![0x21u8; 4096];
    let _ = std::fs::write(source_dir.join("IMEASURE"), &payload);
    let sample = root.join("d00000").join("measured.bin");
    let handle = File::open(root.join("d00000")).unwrap();
    let _ = create_placeholder(
        &handle,
        "measured.bin",
        "IMEASURE",
        payload.len() as u64,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    );
    let outside = base.join("outside.bin");
    let _ = std::fs::write(&outside, &payload);

    let time_open = |path: &Path| -> Duration {
        let started = Instant::now();
        if let Ok(reader) = Reader::start(&exe, path) {
            let _ = reader.get(Duration::from_secs(60));
        }
        started.elapsed()
    };
    // One warm-up so process spawn cost is not what is being compared.
    let _ = time_open(&outside);
    let outside_latency = time_open(&outside);
    let first = time_open(&sample);
    let second = time_open(&sample);
    println!("  first (intercepted, hydrating) open: {first:?}");
    println!("  second (ignore-marked) open:         {second:?}");
    println!("  open of a file outside the root:     {outside_latency:?}");

    let ignore_before = settled("before ignore marks");
    // Ignore-mark a slice of the tree by opening it, then measure.
    let sample_count = 2000.min(files);
    for d in 0..dirs {
        let dir = root.join(format!("d{:05}", d));
        for f in 0..per_dir {
            if d * per_dir + f >= sample_count {
                break;
            }
            let path = dir.join(format!("f{f:04}"));
            let _ = std::fs::write(source_dir.join(format!("I{d}-{f}")), &payload);
            if let Ok(reader) = Reader::start(&exe, &path) {
                let _ = reader.get(Duration::from_secs(30));
            }
        }
        if d * per_dir >= sample_count {
            break;
        }
    }
    let ignored = helper_marks(helper.pid())
        .iter()
        .filter(|m| m.ignored_mask & FAN_OPEN_PERM != 0)
        .count();
    println!("  {ignored} ignore mark(s) after {sample_count} hydrations");
    let ignore_after = settled("with ignore marks in place");
    let per_ignore =
        (ignore_after as i64 - ignore_before as i64) as f64 / ignored.max(1) as f64;
    println!("  per ignore mark, after drop_caches: {per_ignore:.1} B");

    let _ = runtime.block_on(link.unregister_root(root_id));
    helper.stop();
    let _ = std::fs::remove_dir_all(&base);
    0
}
