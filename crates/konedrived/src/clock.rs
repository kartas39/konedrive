//! The machine's wall clock, read in one place.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Unix seconds now; `0` for a clock set before 1970.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs() as i64)
}

/// The time that is `unix_seconds` after 1970; 1970 itself for a time before it.
pub(crate) fn to_system_time(unix_seconds: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(unix_seconds.max(0) as u64)
}
