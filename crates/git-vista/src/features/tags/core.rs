//! The pure display model for the tag list (M2.21b, #236).
//!
//! Framework-free and host-tested, per the `features/*/core.rs` rule: this
//! file must build and run under `cargo test --workspace`, so it holds every
//! decision the tag list makes and the wasm view holds none.
//!
//! # The decision this file exists for
//!
//! `TagDetail` models a lightweight tag's missing tagger and message as
//! `None`, because a lightweight tag genuinely has neither. A view that
//! rendered `Option::unwrap_or_default()` would put an empty "Tagger:" line on
//! screen, which reads as *a tag with a blank tagger* — a different and false
//! claim. So the mapping from "no value" to "what the user sees" is made here,
//! once, where it can be tested: a lightweight tag shows no tagger line at
//! all, while an **annotated** tag with no message shows an explicit
//! [`NO_MESSAGE`] note, because there the absence is worth remarking on.

use git_vista_protocol::dto::{SignatureStatus, TagDetail, TagKind};

/// What an annotated tag with no message body shows instead of a message.
/// Only annotated tags can reach this: a lightweight tag has nowhere to put a
/// message, so its silence needs no explanation.
pub const NO_MESSAGE: &str = "no annotation";

/// How many characters of a tag message the list shows before eliding. The
/// list is a scan-and-pick surface; the whole body belongs on a detail
/// surface, not in every row.
pub const MESSAGE_PREVIEW_CHARS: usize = 96;

/// One row of the tag list, with every decision already made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRow {
    /// The tag's name, as the badge on the graph spells it.
    pub name: String,
    /// `"annotated"` or `"lightweight"` — shown as a pill, so the two kinds
    /// are distinguishable without reading anything else in the row.
    pub kind_label: &'static str,
    /// Whether this is an annotated tag. Kept alongside `kind_label` so a
    /// caller branches on the fact rather than on the display string.
    pub annotated: bool,
    /// The tagged commit's short id — what the row links the tag to.
    pub target_short: String,
    /// The full target id, for a title/tooltip and for menu wiring.
    pub target: String,
    /// The tagger line, verbatim from git. `None` for a lightweight tag, and
    /// the view must then render **no tagger line at all**.
    pub tagger: Option<String>,
    /// The message preview: the first line, elided at
    /// [`MESSAGE_PREVIEW_CHARS`]. `None` for a lightweight tag.
    pub message: Option<String>,
    /// The note to show *in place of* a message. `Some(`[`NO_MESSAGE`]`)` only
    /// for an annotated tag that carries no message; `None` otherwise —
    /// including for every lightweight tag.
    pub message_absent_note: Option<&'static str>,
    /// A short badge for a tag whose object carries a signature, or `None`
    /// when there is nothing signed to say. M2.21b never claims a signature is
    /// *valid*; see [`signature_badge`].
    pub signature_badge: Option<&'static str>,
}

/// What a tag's [`SignatureStatus`] shows as a badge, or `None` for a tag with
/// nothing to say about signing.
///
/// [`SignatureStatus::Unsigned`] deliberately renders **no badge**: most tags
/// are unsigned and a badge on all of them is noise that makes the ones that
/// matter harder to see. Every other status gets wording that does not
/// overclaim — in particular `Unverifiable` says "not checked", never
/// "invalid", because "we could not check" and "we checked and it failed" are
/// different facts and the DTO keeps them apart precisely so a UI does not
/// collapse them.
pub fn signature_badge(status: SignatureStatus) -> Option<&'static str> {
    match status {
        SignatureStatus::Unsigned => None,
        SignatureStatus::Valid => Some("signature valid"),
        SignatureStatus::Invalid => Some("signature invalid"),
        SignatureStatus::UnknownKey => Some("signed, key unknown"),
        SignatureStatus::Unverifiable => Some("signed, not checked"),
    }
}

/// The first line of `message`, elided at [`MESSAGE_PREVIEW_CHARS`]
/// *characters* (not bytes, so a multi-byte character can never be cut in
/// half). `None` when there is no first line worth showing.
fn preview(message: &str) -> Option<String> {
    let first = message.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return None;
    }
    if first.chars().count() <= MESSAGE_PREVIEW_CHARS {
        return Some(first.to_string());
    }
    let head: String = first.chars().take(MESSAGE_PREVIEW_CHARS).collect();
    Some(format!("{head}…"))
}

/// Turn one wire [`TagDetail`] into its display row.
pub fn tag_row(tag: &TagDetail) -> TagRow {
    let annotated = tag.kind == TagKind::Annotated;
    let message = tag.message.as_ref().and_then(|m| preview(m.as_str()));
    let target = tag.target.as_str().to_string();
    TagRow {
        name: tag.name.as_str().to_string(),
        kind_label: if annotated {
            "annotated"
        } else {
            "lightweight"
        },
        annotated,
        target_short: target.chars().take(7).collect(),
        target,
        // A lightweight tag has no tagger; that is `None` here and no line in
        // the view — never `""`, which would render as a blank tagger.
        tagger: tag.tagger.clone(),
        message: message.clone(),
        // Only an annotated tag's missing message is remarkable.
        message_absent_note: (annotated && message.is_none()).then_some(NO_MESSAGE),
        signature_badge: signature_badge(tag.signature),
    }
}

/// The whole list, in the order the server sent it (which is by name).
pub fn tag_rows(tags: &[TagDetail]) -> Vec<TagRow> {
    tags.iter().map(tag_row).collect()
}

/// The empty-state line, so the "no tags" wording is testable and cannot
/// silently become an empty panel that looks like a failed fetch.
pub const NO_TAGS: &str = "No tags in this repository yet.";

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::plan::{CommitOid, TagMessage, TagName};

    fn detail(name: &str, kind: TagKind) -> TagDetail {
        TagDetail {
            name: TagName::new(name).unwrap(),
            kind,
            target: CommitOid::new("a1b2c3d4e5".to_string() + &"0".repeat(30)).unwrap(),
            tag_object: None,
            tagger: None,
            message: None,
            signature: SignatureStatus::Unsigned,
        }
    }

    #[test]
    fn a_lightweight_tag_shows_no_tagger_line_and_no_missing_message_note() {
        let row = tag_row(&detail("tip-marker", TagKind::Lightweight));
        assert_eq!(row.kind_label, "lightweight");
        assert!(!row.annotated);
        assert_eq!(row.tagger, None, "and the view renders no tagger line");
        assert_eq!(row.message, None);
        assert_eq!(
            row.message_absent_note, None,
            "a lightweight tag's silence needs no explanation — only an \
             annotated tag's does"
        );
        assert_eq!(row.target_short, "a1b2c3d");
        assert_eq!(row.target.len(), 40);
    }

    #[test]
    fn an_annotated_tag_with_no_message_says_so_explicitly() {
        let row = tag_row(&detail("v1.0", TagKind::Annotated));
        assert_eq!(row.kind_label, "annotated");
        assert_eq!(row.message, None);
        assert_eq!(row.message_absent_note, Some(NO_MESSAGE));
    }

    #[test]
    fn an_annotated_tag_previews_its_first_line_only() {
        let mut d = detail("v1.0", TagKind::Annotated);
        d.tagger = Some("Ada Lovelace <ada@example.com> 1753300000 +0000".to_string());
        d.message = Some(TagMessage::new("first stable release\n\nlong notes here").unwrap());
        let row = tag_row(&d);
        assert_eq!(row.message.as_deref(), Some("first stable release"));
        assert_eq!(row.message_absent_note, None);
        assert_eq!(
            row.tagger.as_deref(),
            Some("Ada Lovelace <ada@example.com> 1753300000 +0000")
        );
    }

    #[test]
    fn a_long_first_line_is_elided_on_a_character_boundary() {
        let mut d = detail("v-long", TagKind::Annotated);
        let line = "é".repeat(MESSAGE_PREVIEW_CHARS + 20);
        d.message = Some(TagMessage::new(line).unwrap());
        let preview = tag_row(&d).message.unwrap();
        assert_eq!(preview.chars().count(), MESSAGE_PREVIEW_CHARS + 1);
        assert!(preview.ends_with('…'));
        assert!(!preview.contains('\u{FFFD}'));

        // …and a line exactly at the limit is NOT elided, so the ellipsis
        // always means "there is more".
        let mut exact = detail("v-exact", TagKind::Annotated);
        exact.message = Some(TagMessage::new("x".repeat(MESSAGE_PREVIEW_CHARS)).unwrap());
        let preview = tag_row(&exact).message.unwrap();
        assert!(!preview.ends_with('…'));
        assert_eq!(preview.chars().count(), MESSAGE_PREVIEW_CHARS);
    }

    /// The vocabulary must not collapse: "could not check" and "checked and
    /// failed" are different badges, and unsigned gets no badge at all.
    #[test]
    fn every_signature_status_has_its_own_non_overclaiming_badge() {
        assert_eq!(signature_badge(SignatureStatus::Unsigned), None);
        let badges = [
            signature_badge(SignatureStatus::Valid),
            signature_badge(SignatureStatus::Invalid),
            signature_badge(SignatureStatus::UnknownKey),
            signature_badge(SignatureStatus::Unverifiable),
        ];
        assert!(badges.iter().all(Option::is_some));
        let distinct: std::collections::BTreeSet<_> = badges.iter().collect();
        assert_eq!(distinct.len(), 4, "no two statuses may share wording");
        // The specific confusion the DTO exists to prevent.
        let unverifiable = signature_badge(SignatureStatus::Unverifiable).unwrap();
        assert!(
            !unverifiable.contains("invalid"),
            "an unchecked signature must never be worded as an invalid one"
        );
        assert!(
            !unverifiable.contains("valid"),
            "nor as a valid one — {unverifiable:?}"
        );
    }

    #[test]
    fn the_list_keeps_the_servers_order() {
        let tags = [
            detail("a", TagKind::Lightweight),
            detail("m", TagKind::Annotated),
            detail("z", TagKind::Lightweight),
        ];
        assert_eq!(
            tag_rows(&tags)
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "m", "z"]
        );
    }
}
