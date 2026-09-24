//! A Graph `driveItem` as the delta feed, `GET /items/{id}` and the writes
//! return it — only the fields the client uses — and Graph's timestamps.

use serde::Deserialize;

/// Linux's limit on one name, in bytes (OneDrive's is 255
/// characters, and a Cyrillic character is two bytes).
pub const NAME_MAX: usize = 255;

/// The prefix of the daemon's own working names in a folder.
pub const RESERVED_PREFIX: &str = ".konedrive-";

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveItem {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub e_tag: Option<String>,
    #[serde(default)]
    pub c_tag: Option<String>,
    #[serde(default)]
    pub parent_reference: Option<ParentReference>,
    #[serde(default)]
    pub file: Option<FileFacet>,
    #[serde(default)]
    pub folder: Option<serde_json::Value>,
    #[serde(default)]
    pub deleted: Option<serde_json::Value>,
    #[serde(default)]
    pub root: Option<serde_json::Value>,
    #[serde(default)]
    pub remote_item: Option<serde_json::Value>,
    #[serde(default)]
    pub package: Option<serde_json::Value>,
    #[serde(default)]
    pub special_folder: Option<SpecialFolder>,
    #[serde(default)]
    pub file_system_info: Option<FileSystemInfo>,
    #[serde(default)]
    pub last_modified_date_time: Option<String>,
    #[serde(default, rename = "@microsoft.graph.downloadUrl")]
    pub download_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParentReference {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub drive_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileFacet {
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub hashes: Option<Hashes>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hashes {
    #[serde(default)]
    pub quick_xor_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SpecialFolder {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemInfo {
    #[serde(default)]
    pub last_modified_date_time: Option<String>,
}

impl DriveItem {
    /// The time the file was last changed, as the user sees it: the file
    /// system's own time when Graph has it, the item's otherwise, and 0 when
    /// neither can be read.
    pub fn mtime(&self) -> i64 {
        self.file_system_info
            .as_ref()
            .and_then(|f| f.last_modified_date_time.as_deref())
            .or(self.last_modified_date_time.as_deref())
            .and_then(parse_graph_time)
            .unwrap_or(0)
    }

    pub fn quick_xor_hash(&self) -> Option<&str> {
        self.file.as_ref()?.hashes.as_ref()?.quick_xor_hash.as_deref()
    }
}

/// Unix seconds from Graph's UTC timestamps, `2024-05-01T10:00:00Z` or with a
/// fraction (`…00.123Z`, dropped). Anything else is `None`.
pub fn parse_graph_time(value: &str) -> Option<i64> {
    let value = value.strip_suffix('Z')?;
    let (date, time) = value.split_once('T')?;
    let mut d = date.splitn(3, '-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.split('.').next()?.splitn(3, ':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let second: i64 = t.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Graph's UTC timestamp for Unix seconds, `2024-05-01T10:00:00Z`: the
/// inverse of [`parse_graph_time`], for `fileSystemInfo` in writes.
pub fn format_graph_time(seconds: i64) -> String {
    let (days, time) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", time / 3_600, time % 3_600 / 60, time % 60)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
pub(super) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = (if year >= 0 { year } else { year - 399 }) / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The proleptic Gregorian date of a day since 1970-01-01: the inverse of
/// [`days_from_civil`] (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = (if days >= 0 { days } else { days - 146_096 }) / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_times_are_unix_seconds() {
        assert_eq!(parse_graph_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_graph_time("2024-05-01T10:00:00Z"), Some(1_714_557_600));
        assert_eq!(parse_graph_time("2024-05-01T10:00:00.123Z"), Some(1_714_557_600), "fractions are dropped");
        assert_eq!(parse_graph_time("2000-02-29T23:59:59Z"), Some(951_868_799));
        assert_eq!(parse_graph_time("1969-12-31T23:59:59Z"), Some(-1));
    }

    #[test]
    fn unix_seconds_format_back_to_graph_times() {
        for (seconds, text) in [
            (0, "1970-01-01T00:00:00Z"),
            (1_714_557_600, "2024-05-01T10:00:00Z"),
            (951_868_799, "2000-02-29T23:59:59Z"),
            (-1, "1969-12-31T23:59:59Z"),
        ] {
            assert_eq!(format_graph_time(seconds), text);
            assert_eq!(parse_graph_time(text), Some(seconds));
        }
    }

    #[test]
    fn anything_else_is_not_a_graph_time() {
        for bad in ["", "2024-05-01", "2024-05-01T10:00:00", "2024-13-01T00:00:00Z", "2024-05-01T25:00:00Z", "x-05-01T10:00:00Z"] {
            assert_eq!(parse_graph_time(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_delta_item_deserializes_with_every_facet_it_may_carry() {
        let item: DriveItem = serde_json::from_value(serde_json::json!({
            "id": "I", "name": "n", "size": 3, "eTag": "e", "cTag": "c",
            "parentReference": {"id": "P", "driveId": "D"},
            "file": {"mimeType": "image/jpeg", "hashes": {"quickXorHash": "q"}},
            "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"},
            "specialFolder": {"name": "vault"}, "remoteItem": {}, "package": {"type": "oneNote"},
            "deleted": {"state": "deleted"}, "root": {}, "folder": {"childCount": 0}
        }))
        .unwrap();
        assert_eq!(item.parent_reference.as_ref().and_then(|p| p.id.as_deref()), Some("P"));
        assert_eq!(item.file.as_ref().and_then(|f| f.hashes.as_ref()).and_then(|h| h.quick_xor_hash.as_deref()), Some("q"));
        assert_eq!(item.special_folder.as_ref().and_then(|s| s.name.as_deref()), Some("vault"));
        assert!(item.remote_item.is_some() && item.package.is_some() && item.deleted.is_some() && item.root.is_some());
    }
}
