use std::os::unix::fs::PermissionsExt;

use super::*;

fn fake(dir: &Path) -> (Baloo, PathBuf) {
    let log = dir.join("calls");
    let script = dir.join("balooctl6");
    std::fs::write(&script, format!("#!/bin/sh\necho \"$@\" >> '{}'\n", log.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (Baloo { program: Some(script), settings: None, timeout: DEFAULT_TIMEOUT }, log)
}

fn home(name: &str) -> Option<String> {
    (name == "HOME").then(|| "/home/u".to_owned())
}

#[tokio::test]
async fn the_folder_is_excluded_and_included_again() {
    let dir = tempfile::tempdir().unwrap();
    let (baloo, log) = fake(dir.path());
    assert!(baloo.exclude(Path::new("/home/u/OneDrive")).await);
    baloo.include_again(Path::new("/home/u/OneDrive")).await;
    assert_eq!(
        std::fs::read_to_string(log).unwrap(),
        "config add excludeFolders /home/u/OneDrive\nconfig rm excludeFolders /home/u/OneDrive\n"
    );
}

#[tokio::test]
async fn a_missing_program_is_not_an_error() {
    let baloo = Baloo { program: Some(PathBuf::from("/nonexistent/balooctl6")), settings: None, timeout: DEFAULT_TIMEOUT };
    assert!(!baloo.exclude(Path::new("/home/u/OneDrive")).await);
    baloo.include_again(Path::new("/home/u/OneDrive")).await;
    assert!(!baloo.is_excluded(Path::new("/home/u/OneDrive")).await);
}

/// A fake `balooctl6` that hangs (blocked on an unresponsive Baloo
/// D-Bus service, say) must not block registration or Forget forever:
/// `timeout` cuts it off, `kill_on_drop` makes sure the child does not
/// linger, and a timeout is treated exactly like a missing program.
#[tokio::test]
async fn a_hung_program_is_killed_and_treated_as_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("balooctl6");
    std::fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let baloo = Baloo { program: Some(script.clone()), settings: None, timeout: Duration::from_millis(100) };

    let start = std::time::Instant::now();
    assert!(!baloo.exclude(Path::new("/home/u/OneDrive")).await);
    assert!(!baloo.is_excluded(Path::new("/home/u/OneDrive")).await);
    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}, the 5s sleep was not cut off");

    // `kill_on_drop` must have actually killed the child, not just given
    // up waiting for it: nothing matching the fake script is left
    // running (give the kernel a moment to reap it).
    for _ in 0..20 {
        let still_running = std::process::Command::new("pgrep")
            .args(["-f", &script.display().to_string()])
            .output()
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(false);
        if !still_running {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the hung balooctl6 was not killed");
}

#[tokio::test]
async fn a_disabled_baloo_runs_no_program_at_all() {
    let dir = tempfile::tempdir().unwrap();
    // Points at a script that would prove it ran, if it ran.
    let (_unused, log) = fake(dir.path());
    let baloo = Baloo::disabled();
    assert!(!baloo.exclude(Path::new("/home/u/OneDrive")).await);
    assert!(!baloo.is_excluded(Path::new("/home/u/OneDrive")).await);
    baloo.include_again(Path::new("/home/u/OneDrive")).await;
    assert!(!log.exists(), "no program ran");
}

/// The exclusions come from `baloofilerc` itself — the user's own
/// line exactly as found on their machine, where `balooctl6 config list
/// excludeFolders` printed an empty list — in every form KConfig writes.
#[test]
fn the_exclusions_are_read_from_baloofilerc_as_kconfig_writes_them() {
    let text = "[Basic Settings]\nexclude folders=/not/general\n\
                [General]\nexclude folders[$e]=$HOME/tools/test/\nfolders[$e]=$HOME/\n";
    assert_eq!(excluded_folders(text, &home), [PathBuf::from("/home/u/tools/test")]);

    let text = "[General]\nexclude folders[$e]=${HOME}/a/,/srv/b\\,c/,relative/$UNSET,$$HOME/x\n\
                [General][Nested]\nexclude folders=/nested\n";
    assert_eq!(excluded_folders(text, &home), [PathBuf::from("/home/u/a"), PathBuf::from("/srv/b,c")]);
    assert_eq!(excluded_folders("[General]\nexclude folders=/plain/\n", &home), [PathBuf::from("/plain")]);
}

/// Read from the file a `Baloo` names — never `~/.config` in a test —
/// with a folder counted as excluded only inside an excluded directory,
/// not beside one that shares its first letters.
#[tokio::test]
async fn is_excluded_reads_the_settings_file_and_respects_path_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("baloofilerc");
    let baloo = Baloo { program: None, settings: Some(settings.clone()), timeout: DEFAULT_TIMEOUT };
    assert!(!baloo.is_excluded(Path::new("/srv/u/OneDrive")).await, "no file, nothing excluded");

    std::fs::write(&settings, "[General]\nexclude folders[$e]=/srv/u/,/srv/other\n").unwrap();
    assert!(baloo.is_excluded(Path::new("/srv/u/OneDrive")).await, "inside an excluded directory");
    assert!(baloo.is_excluded(Path::new("/srv/u")).await);
    assert!(!baloo.is_excluded(Path::new("/srv/uu/OneDrive")).await, "a shared prefix is not inside");
}

#[test]
fn the_settings_file_is_under_xdg_config_home_or_home() {
    let file = |xdg: Option<&str>, home: Option<&str>| settings_file(xdg.map(Into::into), home.map(Into::into));
    assert_eq!(file(Some("/x/cfg"), Some("/home/u")), Some(PathBuf::from("/x/cfg/baloofilerc")));
    assert_eq!(file(Some("relative"), Some("/home/u")), Some(PathBuf::from("/home/u/.config/baloofilerc")));
    assert_eq!(file(None, Some("/home/u")), Some(PathBuf::from("/home/u/.config/baloofilerc")));
    assert_eq!(file(None, None), None);
}
