//! `settings`: what every account does on a metered connection and on battery.

/// `settings on-metered`'s answer.
pub fn on_metered_text(pause: bool) -> &'static str {
    if pause {
        "On a metered connection: pause."
    } else {
        "On a metered connection: sync as usual."
    }
}

/// `settings on-battery`'s answer, for `sync`, `power-saver` or `pause`.
pub fn on_battery_text(choice: &str) -> String {
    match choice {
        "sync" => "On battery: sync as usual.".to_owned(),
        "power-saver" => "On battery: pause in power-saver mode.".to_owned(),
        "pause" => "On battery: pause.".to_owned(),
        other => format!("On battery: {other}."),
    }
}
