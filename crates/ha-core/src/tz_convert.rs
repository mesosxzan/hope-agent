//! JSON response timestamp conversion.
//!
//! Converts UTC RFC3339 timestamps in API JSON responses to the user's
//! effective timezone. This is applied as a post-processing step in the
//! HTTP server layer so that web/Docker clients see times in their local
//! zone instead of raw UTC.

use serde_json::Value;

/// Recursively walk a JSON value and convert any string that looks like a
/// UTC RFC3339 timestamp to the given IANA timezone.
///
/// Recognised patterns:
/// - `"2026-06-27T01:30:00+00:00"`  (chrono Utc output)
/// - `"2026-06-27T01:30:00Z"`       (Z suffix)
/// - `"2026-06-27T01:30:00.123456+00:00"` (with sub-second precision)
///
/// Strings without timezone info (e.g. SQLite `datetime('now')` output
/// `"2026-06-27 01:30:00"`) are also converted — they are assumed to be
/// UTC (which is how SQLite stores them).
pub fn convert_timestamps_to_local(value: &mut Value, tz_name: &str) {
    // Parse the target timezone once.
    let tz: Option<chrono_tz::Tz> = tz_name.parse().ok();
    if tz.is_none() {
        // Unknown zone — nothing to convert.
        return;
    }
    let tz = tz.unwrap();

    match value {
        Value::String(s) => {
            if let Some(converted) = try_convert_timestamp(s, &tz) {
                *s = converted;
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                convert_timestamps_to_local(item, tz_name);
            }
        }
        Value::Object(map) => {
            for (_key, v) in map.iter_mut() {
                convert_timestamps_to_local(v, tz_name);
            }
        }
        _ => {}
    }
}

/// Try to convert a single timestamp string from UTC to the given timezone.
/// Returns `Some(converted_string)` if the input looks like a timestamp,
/// `None` otherwise.
fn try_convert_timestamp(s: &str, tz: &chrono_tz::Tz) -> Option<String> {
    // Fast reject: must start with a digit (year) and be at least 19 chars
    // (YYYY-MM-DDTHH:MM:SS).
    if s.is_empty() || !s.as_bytes()[0].is_ascii_digit() || s.len() < 19 {
        return None;
    }

    // Try RFC3339 with timezone offset (chrono output).
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        let local = dt.with_timezone(tz);
        return Some(local.to_rfc3339());
    }

    // Try SQLite datetime('now') format: "2026-06-27 01:30:00" (no T, no tz).
    // These are UTC. Replace space with T and append +00:00.
    if s.len() == 19 && s.as_bytes()[10] == b' ' {
        let fixed = format!("{}+00:00", s.replace(' ', "T"));
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&fixed) {
            let local = dt.with_timezone(tz);
            return Some(local.to_rfc3339());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_rfc3339_utc_to_shanghai() {
        let mut v = Value::String("2026-06-27T01:30:00+00:00".to_string());
        convert_timestamps_to_local(&mut v, "Asia/Shanghai");
        // 01:30 UTC = 09:30 Shanghai
        assert_eq!(v.as_str().unwrap(), "2026-06-27T09:30:00+08:00");
    }

    #[test]
    fn converts_z_suffix_to_shanghai() {
        let mut v = Value::String("2026-06-27T01:30:00Z".to_string());
        convert_timestamps_to_local(&mut v, "Asia/Shanghai");
        assert_eq!(v.as_str().unwrap(), "2026-06-27T09:30:00+08:00");
    }

    #[test]
    fn converts_subsecond_precision() {
        let mut v = Value::String("2026-06-27T01:30:00.123456+00:00".to_string());
        convert_timestamps_to_local(&mut v, "Asia/Shanghai");
        assert_eq!(v.as_str().unwrap(), "2026-06-27T09:30:00.123456+08:00");
    }

    #[test]
    fn converts_sqlite_datetime_format() {
        let mut v = Value::String("2026-06-27 01:30:00".to_string());
        convert_timestamps_to_local(&mut v, "Asia/Shanghai");
        assert_eq!(v.as_str().unwrap(), "2026-06-27T09:30:00+08:00");
    }

    #[test]
    fn leaves_non_timestamp_strings_untouched() {
        let mut v = Value::String("hello world".to_string());
        convert_timestamps_to_local(&mut v, "Asia/Shanghai");
        assert_eq!(v.as_str().unwrap(), "hello world");
    }

    #[test]
    fn leaves_short_strings_untouched() {
        let mut v = Value::String("2026".to_string());
        convert_timestamps_to_local(&mut v, "Asia/Shanghai");
        assert_eq!(v.as_str().unwrap(), "2026");
    }

    #[test]
    fn converts_nested_object_timestamps() {
        let mut v = serde_json::json!({
            "created_at": "2026-06-27T01:30:00+00:00",
            "title": "my session",
            "nested": {
                "timestamp": "2026-06-27T02:00:00Z",
                "count": 42
            }
        });
        convert_timestamps_to_local(&mut v, "Asia/Shanghai");
        assert_eq!(v["created_at"], "2026-06-27T09:30:00+08:00");
        assert_eq!(v["title"], "my session");
        assert_eq!(v["nested"]["timestamp"], "2026-06-27T10:00:00+08:00");
        assert_eq!(v["nested"]["count"], 42);
    }

    #[test]
    fn converts_array_timestamps() {
        let mut v = serde_json::json!(["2026-06-27T01:30:00+00:00", "not a timestamp", 42]);
        convert_timestamps_to_local(&mut v, "Asia/Shanghai");
        assert_eq!(v[0], "2026-06-27T09:30:00+08:00");
        assert_eq!(v[1], "not a timestamp");
        assert_eq!(v[2], 42);
    }

    #[test]
    fn no_op_for_unknown_timezone() {
        let mut v = Value::String("2026-06-27T01:30:00+00:00".to_string());
        convert_timestamps_to_local(&mut v, "Invalid/Zone");
        // Unchanged — unknown zone
        assert_eq!(v.as_str().unwrap(), "2026-06-27T01:30:00+00:00");
    }

    #[test]
    fn no_op_for_utc_timezone() {
        let mut v = Value::String("2026-06-27T01:30:00+00:00".to_string());
        convert_timestamps_to_local(&mut v, "UTC");
        // UTC → UTC, still converts (offset changes from +00:00 to Z or stays)
        // The important thing is it doesn't crash or corrupt.
        assert!(v.as_str().unwrap().contains("2026-06-27T01:30:00"));
    }
}
