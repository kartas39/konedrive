//! The machine's wall clock, read in one place.

use std::time::{SystemTime, UNIX_EPOCH};

/// Unix seconds now; `0` for a clock set before 1970.
pub fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs() as i64)
}
