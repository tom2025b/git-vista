//! Formatting commit timestamps for the labels.
//!
//! Commits carry their time as a Unix timestamp (`CommitSummary::time`, seconds).
//! We show it in the **viewer's local timezone** — for a personal history viewer
//! that's what "when was this committed" means to the reader — using the JS
//! `Date` to break the instant down (correct per-commit, including DST). The
//! actual string assembly is a pure, host-tested helper so the formatting is
//! pinned independently of the wasm-only date plumbing.

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Format broken-down local date/time as a compact, US-readable label:
/// `"Jun 29 2:32 PM"` (12-hour clock with AM/PM). The year is shown only when it
/// isn't `current_year` (e.g. `"Jun 29 2024 2:32 PM"`), so the common case stays
/// short. `month`/`day` are 1-based; `hour` is given in 24-hour form (0–23).
/// Pure and host-testable.
pub fn format_label(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    current_year: i32,
) -> String {
    let mon = MONTHS
        .get(month.saturating_sub(1) as usize)
        .copied()
        .unwrap_or("???");
    let period = if hour < 12 { "AM" } else { "PM" };
    // 24h -> 12h: 0 and 12 both display as 12; hour is not zero-padded.
    let h12 = match hour % 12 {
        0 => 12,
        h => h,
    };
    if year == current_year {
        format!("{mon} {day} {h12}:{minute:02} {period}")
    } else {
        format!("{mon} {day} {year} {h12}:{minute:02} {period}")
    }
}

/// Compact local-timezone label for a Unix timestamp (seconds), e.g.
/// `"Jun 29 14:32"`. wasm-only: it uses the JS `Date`, which resolves the
/// browser's timezone (and DST) for the commit's own instant, and the current
/// year (so older commits show their year).
#[cfg(target_arch = "wasm32")]
pub fn local_timestamp(epoch_secs: i64) -> String {
    // JS `Date` takes milliseconds since the epoch.
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(epoch_secs as f64 * 1000.0));
    let current_year = js_sys::Date::new_0().get_full_year() as i32;
    format_label(
        d.get_full_year() as i32,
        d.get_month() + 1, // JS months are 0-based
        d.get_date(),
        d.get_hours(),
        d.get_minutes(),
        current_year,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_year_hides_the_year() {
        assert_eq!(format_label(2026, 6, 29, 14, 32, 2026), "Jun 29 2:32 PM");
    }

    #[test]
    fn other_year_shows_the_year() {
        assert_eq!(format_label(2024, 6, 29, 14, 32, 2026), "Jun 29 2024 2:32 PM");
    }

    #[test]
    fn morning_is_am_and_day_is_unpadded() {
        // Single-digit day reads naturally unpadded; minutes stay zero-padded.
        assert_eq!(format_label(2026, 1, 5, 4, 7, 2026), "Jan 5 4:07 AM");
    }

    #[test]
    fn midnight_and_noon_are_twelve() {
        assert_eq!(format_label(2026, 12, 31, 0, 0, 2026), "Dec 31 12:00 AM");
        assert_eq!(format_label(2026, 7, 1, 12, 0, 2026), "Jul 1 12:00 PM");
    }

    #[test]
    fn late_evening_is_pm() {
        assert_eq!(format_label(2026, 7, 1, 23, 5, 2026), "Jul 1 11:05 PM");
    }
}
