/// `label` as one word of a shell command: as it is when it needs no quoting, in single
/// quotes otherwise.
pub fn shell_word(label: &str) -> String {
    if !label.is_empty() && label.chars().all(|c| c.is_alphanumeric() || "._-+,:".contains(c)) {
        label.to_owned()
    } else {
        format!("'{}'", label.replace('\'', r"'\''"))
    }
}

/// `text` with every line that is not empty indented by two spaces: one account's block in
/// `status` and `sync status` when they show several.
pub fn indented(text: &str) -> String {
    text.lines().map(|line| if line.is_empty() { "\n".to_owned() } else { format!("  {line}\n") }).collect()
}

/// A line of the error output that is no failure: the command did what it was asked.
pub fn warning_text(text: &str) -> String {
    format!("warning: {text}")
}

/// A path given to a command that is not there.
pub fn no_such_path_text(path: &str) -> String {
    format!("no such path: {path}")
}

/// A path given to a command that is not UTF-8: the daemon takes paths as text.
pub const NOT_UTF8_PATH: &str = "non-UTF-8 path";

/// A relative path given with no current directory to make it absolute from.
pub const NOT_ABSOLUTE: &str = "cannot make the path absolute";

/// `40 s`, `2 min`, `3 h 5 min`.
pub(crate) fn seconds_text(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds} s"),
        60..=3_599 => format!("{} min", seconds / 60),
        _ if seconds % 3_600 < 60 => format!("{} h", seconds / 3_600),
        _ => format!("{} h {} min", seconds / 3_600, seconds % 3_600 / 60),
    }
}

/// `45 678`: the thousands set apart.
pub(crate) fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// A duration as `sync pause --for` takes it: `90s`, `30m`, `2h`, `1d`, or
/// several at once (`1h30m`); a bare number is seconds. `None` for anything
/// else, for zero, and for more than a `u32` of seconds.
pub fn parse_duration(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(seconds) = text.parse::<u64>() {
        return u32::try_from(seconds).ok().filter(|&s| s > 0);
    }
    let (mut total, mut number) = (0u64, String::new());
    for c in text.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return None,
        };
        let value: u64 = std::mem::take(&mut number).parse().ok()?;
        total = total.checked_add(value.checked_mul(unit)?)?;
    }
    if !number.is_empty() {
        return None;
    }
    u32::try_from(total).ok().filter(|&s| s > 0)
}

/// `text` with its first letter in capitals, to stand as a sentence of its own.
pub(crate) fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// When the folder was last checked with OneDrive, as `sync status` says it
///: "20 s ago", "5 min ago", "3 h ago", "2 d ago", or "never"
/// for 0. `last` and `now` are unix seconds.
pub fn checked_text(last: i64, now: i64) -> String {
    if last == 0 {
        return "never".to_owned();
    }
    match now - last {
        ago if ago < 0 => "just now".to_owned(),
        ago @ 0..=59 => format!("{ago} s ago"),
        ago @ 60..=3_599 => format!("{} min ago", ago / 60),
        ago @ 3_600..=86_399 => format!("{} h ago", ago / 3_600),
        ago => format!("{} d ago", ago / 86_400),
    }
}

/// Unix seconds now.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `2026-09-24 10:00:05`, in this machine's time zone (`TZ` as the C library
/// reads it).
pub fn local_time(at: i64) -> String {
    let seconds = at as libc::time_t;
    // SAFETY: `tm` is plain data that `localtime_r` fills in; both pointers
    // are to live locals for the length of the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
        return at.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests;
