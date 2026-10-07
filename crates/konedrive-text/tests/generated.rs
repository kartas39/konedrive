//! The generated C++ in git is what the generator writes now.

use std::path::Path;

/// Every generated file is written again from the catalogue and compared
/// with the one in git. Run with `KONEDRIVE_UPDATE_GENERATED=1`, the files
/// are rewritten instead.
#[test]
fn the_generated_files_in_git_are_current() {
    let top = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let update = std::env::var_os("KONEDRIVE_UPDATE_GENERATED").is_some_and(|value| value == "1");
    let mut stale = Vec::new();
    for file in konedrive_text::cpp::files() {
        let path = top.join(file.path);
        if update {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &file.content).unwrap_or_else(|e| panic!("{}: {e}", file.path));
            continue;
        }
        if std::fs::read_to_string(&path).ok().as_deref() != Some(file.content.as_str()) {
            stale.push(file.path);
        }
    }
    assert!(
        stale.is_empty(),
        "not what the catalogue gives now: {stale:?}. Run `KONEDRIVE_UPDATE_GENERATED=1 cargo test -p konedrive-text` and commit the files."
    );
}
