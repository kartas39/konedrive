use super::*;

#[tokio::test]
async fn thumbnails_go_only_while_on_and_nothing_stops() {
    let running = Running::default();
    assert!(running.thumbnails_go());
    running.change(|s| s.thumbnails = false);
    assert!(!running.thumbnails_go());
    assert!(!running.stopped(), "thumbnails off stop nothing else");
    running.change(|s| s.thumbnails = true);
    tokio::time::timeout(std::time::Duration::from_secs(1), running.thumbnails_turned_on()).await.expect("turned on wakes the filler");
    running.change(|s| s.paused_until = Some(0));
    assert_eq!(running.stop(), Some(Stop::Paused(0)));
    assert!(!running.thumbnails_go(), "a pause stops thumbnails too");
}

/// Each `on_battery` choice against on battery or on mains, in the
/// power-saver profile or another — on mains the battery never holds — and the
/// metered connection, which `pause_on_metered = false` ignores and which wins over a
/// battery reason.
#[test]
fn the_hold_follows_the_conditions_and_the_settings() {
    let settings = |on_battery| HoldSettings { on_battery, ..HoldSettings::default() };
    let conditions = |on_battery, power_saver| Conditions { metered: false, on_battery, power_saver };
    for (choice, on_battery, power_saver, held) in [
        (OnBattery::Sync, true, true, None),
        (OnBattery::Sync, true, false, None),
        (OnBattery::PowerSaver, true, true, Some(Hold::PowerSaver)),
        (OnBattery::PowerSaver, true, false, None),
        (OnBattery::Pause, true, true, Some(Hold::OnBattery)),
        (OnBattery::Pause, true, false, Some(Hold::OnBattery)),
    ] {
        assert_eq!(Hold::of(settings(choice), conditions(on_battery, power_saver)), held, "{choice:?}, power-saver {power_saver}");
        for power_saver in [true, false] {
            assert_eq!(Hold::of(settings(choice), conditions(false, power_saver)), None, "{choice:?} on mains");
        }
    }
    let metered = Conditions { metered: true, on_battery: true, power_saver: true };
    assert_eq!(Hold::of(settings(OnBattery::Pause), metered), Some(Hold::Metered), "the network's reason first");
    let ignoring = HoldSettings { pause_on_metered: false, on_battery: OnBattery::Sync };
    assert_eq!(Hold::of(ignoring, metered), None, "pause_on_metered = false");
}

/// `SyncAnyway` lifts the hold until the conditions change, or the hold's own settings
/// do; the thumbnail setting and conditions told again unchanged leave it lifted.
#[tokio::test]
async fn sync_anyway_lasts_until_a_source_or_the_holds_settings_change() {
    let running = Running::default();
    let metered = Conditions { metered: true, ..Conditions::default() };
    assert!(running.set_conditions(metered));
    assert_eq!(running.stop(), Some(Stop::Held(Hold::Metered)));
    running.sync_anyway();
    assert_eq!(running.held(), None);
    assert!(!running.set_conditions(metered), "the same again is no change");
    running.change(|s| s.thumbnails = false);
    assert_eq!(running.held(), None);
    running.set_conditions(Conditions { on_battery: true, ..metered });
    assert_eq!(running.held(), Some(Hold::Metered), "a source changed: worked out again");
    running.sync_anyway();
    assert!(!running.set_hold_settings(HoldSettings::default()), "the same again is no change");
    assert_eq!(running.held(), None);
    assert!(running.set_hold_settings(HoldSettings { on_battery: OnBattery::Pause, ..HoldSettings::default() }));
    assert_eq!(running.held(), Some(Hold::Metered), "the hold's setting changed");
    running.change(|s| s.paused_until = Some(0));
    assert_eq!(running.stop(), Some(Stop::Paused(0)), "the user's pause is said first");
}

/// Thumbnails from the account's section, the hold's settings from the global keys,
/// each with its default when absent; an unknown `on_battery` falls back.
#[test]
fn settings_read_config_toml_with_their_defaults() {
    let mut account: AccountConfig = toml::from_str("id = \"a\"\nlabel = \"A\"\n").unwrap();
    assert_eq!(Settings::of(&account), Settings { thumbnails: true, paused_until: None });
    account.thumbnails = Some(false);
    account.paused_until = Some(0);
    assert_eq!(Settings::of(&account), Settings { thumbnails: false, paused_until: Some(0) });

    let read = |text: &str| HoldSettings::of(&toml::from_str::<Config>(&format!("config_version = 2\n{text}")).unwrap());
    assert_eq!(read(""), HoldSettings { pause_on_metered: true, on_battery: OnBattery::PowerSaver });
    assert_eq!(read("pause_on_metered = false\non_battery = \"pause\""), HoldSettings { pause_on_metered: false, on_battery: OnBattery::Pause });
    assert_eq!(read("on_battery = \"sync\"").on_battery, OnBattery::Sync);
    assert_eq!(read("on_battery = \"whenever\"").on_battery, OnBattery::PowerSaver, "an unknown value falls back");
}

/// A pause until resumed stands whatever the time; a timed one stands until its time by
/// the account's clock, and is no pause from then on — one read after a restart that
/// outlasted it never was.
#[test]
fn a_timed_pause_is_over_by_the_accounts_clock() {
    struct At(std::sync::atomic::AtomicI64);
    impl Clock for At {
        fn now(&self) -> i64 {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn sleep_until(&self, _at: i64) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(std::future::pending())
        }
    }
    let clock = Arc::new(At(std::sync::atomic::AtomicI64::new(1_000)));
    let running = Running::new(Settings { paused_until: Some(0), ..Settings::default() }, clock.clone());
    clock.0.store(9_000, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(running.user_pause(), Some(0), "until resumed");

    running.change(|s| s.paused_until = Some(9_100));
    assert_eq!(running.stop(), Some(Stop::Paused(9_100)));
    clock.0.store(9_100, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(running.user_pause(), None, "run out");
    assert_eq!(running.settings().paused_until, None, "and taken off the settings in memory");

    let late = Running::new(Settings { paused_until: Some(500), ..Settings::default() }, clock);
    assert!(!late.stopped(), "a time that passed while the daemon was not running");
}
