//! Formatting commit timestamps for the labels.
//!
//! Commits carry their time as a Unix timestamp (`CommitSummary::time`, seconds).
//! We show it in the **viewer's local timezone** — for a personal history viewer
//! that's what "when was this committed" means to the reader — using the JS
//! `Date` to break the instant down (correct per-commit, including DST). The
//! actual string assembly is a pure, host-tested helper so the formatting is
//! pinned independently of the wasm-only date plumbing.

/// Format already-broken-down local date/time parts as `"YYYY-MM-DD HH:MM"`.
/// Pure and host-testable; `month`/`day` are 1-based, `hour` is 24-hour.
pub fn format_ymd_hm(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> String {
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Local-timezone `"YYYY-MM-DD HH:MM"` for a Unix timestamp (seconds). wasm-only:
/// it uses the JS `Date`, which resolves the browser's timezone (and DST) for the
/// commit's own instant.
#[cfg(target_arch = "wasm32")]
pub fn local_timestamp(epoch_secs: i64) -> String {
    // JS `Date` takes milliseconds since the epoch.
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(epoch_secs as f64 * 1000.0));
    format_ymd_hm(
        d.get_full_year() as i32,
        d.get_month() + 1, // JS months are 0-based
        d.get_date(),
        d.get_hours(),
        d.get_minutes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_pads_every_field() {
        assert_eq!(format_ymd_hm(2026, 6, 9, 4, 7), "2026-06-09 04:07");
    }

    #[test]
    fn keeps_wide_fields_intact() {
        assert_eq!(format_ymd_hm(2026, 12, 31, 23, 59), "2026-12-31 23:59");
    }

    #[test]
    fn midnight_is_zeroes_not_blank() {
        assert_eq!(format_ymd_hm(1999, 1, 1, 0, 0), "1999-01-01 00:00");
    }
}
