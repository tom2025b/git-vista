//! Small, pure text helpers for commit labels.
//!
//! Kept out of [`crate::app`] (and free of any Leptos/DOM dependency) so the
//! string logic can be unit-tested on the host, like [`crate::geometry`].

/// Truncate `s` to at most `max` characters, appending a single-character
/// ellipsis (`…`) when it had to be cut. Counts by `char`, so multi-byte text is
/// never split mid-codepoint. The result is at most `max` characters wide.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        // Reserve one char for the ellipsis so the visible width stays `max`.
        let kept: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_strings_pass_through_unchanged() {
        assert_eq!(truncate("short", 60), "short");
        assert_eq!(truncate("", 60), "");
    }

    #[test]
    fn exact_length_is_not_truncated() {
        let s = "x".repeat(60);
        assert_eq!(truncate(&s, 60), s);
    }

    #[test]
    fn long_strings_are_cut_with_an_ellipsis_and_stay_within_max() {
        let out = truncate(&"x".repeat(100), 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with('…'));
        assert_eq!(out, format!("{}…", "x".repeat(9)));
    }

    #[test]
    fn counts_chars_not_bytes() {
        // Five 2-byte chars; max 5 leaves it untouched (not split by byte length).
        assert_eq!(truncate("café!", 5), "café!");
    }
}
