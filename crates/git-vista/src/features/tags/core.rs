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

/// The in-flight line, shown while the fetch has not answered yet. Distinct
/// from [`NO_TAGS`] on purpose: "we have not asked yet" and "we asked and
/// there are none" are different facts, and collapsing them would tell a user
/// with tags that they have none.
pub const LOADING_TAGS: &str = "Loading tags…";

/// Everything the Tags section can be showing, with the decision already made.
///
/// # Why this enum exists rather than a `match` in the view
///
/// The view lives in `activity.rs`, which is `#[cfg(target_arch = "wasm32")]`
/// — it is never compiled by `cargo test --workspace` and there is no
/// wasm-side test harness, so anything decided there is checked by nothing but
/// the compiler. A `match` on `Option<Result<Vec<TagDetail>, String>>` written
/// in the view could swap two arms (every populated list rendered as the empty
/// state, say) and still build, lint and pass the whole suite. Classifying
/// here instead leaves the view a one-to-one variant→element mapping with no
/// branch of its own to get wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagListView {
    /// The fetch has not answered yet — show [`LOADING_TAGS`].
    Loading,
    /// The fetch failed. The payload is the finished user-facing line, so the
    /// view does no formatting either.
    Failed(String),
    /// The repository answered, with no tags — show [`NO_TAGS`].
    Empty,
    /// One row per tag, in the server's order.
    Rows(Vec<TagRow>),
}

/// Classify what the tag resource currently holds.
///
/// `state` is the Activity panel's resource after `.flatten()`: `None` while
/// the panel's fetch is unresolved (or the panel is shut), `Some(Err)` for a
/// failed fetch, `Some(Ok)` for an answer.
pub fn tag_list_view(state: Option<Result<Vec<TagDetail>, String>>) -> TagListView {
    match state {
        None => TagListView::Loading,
        Some(Err(e)) => TagListView::Failed(format!("Couldn't load tags: {e}")),
        Some(Ok(tags)) if tags.is_empty() => TagListView::Empty,
        Some(Ok(tags)) => TagListView::Rows(tag_rows(&tags)),
    }
}

/// One line rendered *under* a tag's headline row, already decided.
///
/// The variant is what the line means; the view only picks an element and a
/// class for it. Crucially there is no `Option` left for the view to unwrap:
/// a line that should not appear is simply not in the vector, which is what
/// makes "a lightweight tag shows no tagger line" a host-testable fact instead
/// of a comment next to an `Option::map` no test ever runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagRowLine {
    /// The tagger line, verbatim from git. Annotated tags only.
    Tagger(String),
    /// The message preview.
    Message(String),
    /// The stand-in for a message an annotated tag does not have.
    Absent(&'static str),
}

impl TagRowLine {
    /// The text to render.
    pub fn text(&self) -> &str {
        match self {
            TagRowLine::Tagger(t) => t,
            TagRowLine::Message(m) => m,
            TagRowLine::Absent(n) => n,
        }
    }

    /// Whether the line is secondary (rendered muted). The message itself is
    /// the content; the tagger and the absence note are annotations on it.
    pub fn muted(&self) -> bool {
        match self {
            TagRowLine::Message(_) => false,
            TagRowLine::Tagger(_) | TagRowLine::Absent(_) => true,
        }
    }
}

/// The lines under one tag's headline, in render order.
///
/// Every absent field yields **no line at all** — never an empty one. That is
/// the whole point of `TagDetail` modelling absence as `null`: an empty
/// "Tagger" line on screen claims a tag with a blank tagger, which is a
/// different and false statement from "this kind of tag has no tagger".
pub fn tag_row_lines(row: &TagRow) -> Vec<TagRowLine> {
    let mut lines = Vec::new();
    if let Some(tagger) = &row.tagger {
        lines.push(TagRowLine::Tagger(tagger.clone()));
    }
    if let Some(message) = &row.message {
        lines.push(TagRowLine::Message(message.clone()));
    }
    if let Some(note) = row.message_absent_note {
        lines.push(TagRowLine::Absent(note));
    }
    lines
}

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

    /// The four states must stay four states. Written as a table so a mutation
    /// that collapses two arms (the classic: an unresolved fetch rendering as
    /// "no tags", telling a user with tags that they have none) fails on the
    /// pair rather than on one case that happened to be checked.
    #[test]
    fn each_fetch_state_classifies_to_its_own_view() {
        assert_eq!(tag_list_view(None), TagListView::Loading);

        assert_eq!(
            tag_list_view(Some(Err("HTTP 500".to_string()))),
            TagListView::Failed("Couldn't load tags: HTTP 500".to_string()),
            "the error text has to reach the line, or every failure reads alike"
        );

        assert_eq!(tag_list_view(Some(Ok(Vec::new()))), TagListView::Empty);

        let one = vec![detail("v1.0", TagKind::Annotated)];
        match tag_list_view(Some(Ok(one))) {
            TagListView::Rows(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].name, "v1.0");
            }
            other => panic!("a populated answer must render rows, got {other:?}"),
        }

        // The pairs that must never be conflated, stated as inequalities so
        // the intent survives a rename of any single variant.
        assert_ne!(
            tag_list_view(None),
            tag_list_view(Some(Ok(Vec::new()))),
            "'not asked yet' and 'asked, none' are different facts"
        );
        assert_ne!(
            tag_list_view(Some(Err("boom".to_string()))),
            tag_list_view(Some(Ok(Vec::new()))),
            "a failed fetch must never look like an empty repository"
        );
        assert_ne!(
            LOADING_TAGS, NO_TAGS,
            "…and their wording must differ too, or the enum split buys nothing"
        );
    }

    /// The regression the whole `None`-vs-`""` design exists to prevent, moved
    /// somewhere a test can actually run: a lightweight tag contributes **no**
    /// sub-lines, so nothing can render as a blank tagger.
    #[test]
    fn a_lightweight_tag_contributes_no_lines_at_all() {
        let row = tag_row(&detail("tip-marker", TagKind::Lightweight));
        assert_eq!(
            tag_row_lines(&row),
            Vec::new(),
            "an absent field must produce no element, never an empty one"
        );
    }

    #[test]
    fn an_annotated_tag_lines_up_tagger_then_message() {
        let mut d = detail("v1.0", TagKind::Annotated);
        d.tagger = Some("Ada Lovelace <ada@example.com> 1753300000 +0000".to_string());
        d.message = Some(TagMessage::new("first stable release\n\nnotes").unwrap());
        let lines = tag_row_lines(&tag_row(&d));
        assert_eq!(
            lines,
            vec![
                TagRowLine::Tagger("Ada Lovelace <ada@example.com> 1753300000 +0000".to_string()),
                TagRowLine::Message("first stable release".to_string()),
            ]
        );
        assert!(
            !lines[1].muted(),
            "the message is the content, not an aside"
        );
        assert!(lines[0].muted());
        assert_eq!(lines[1].text(), "first stable release");
    }

    #[test]
    fn an_annotated_tag_with_no_message_shows_the_note_in_its_place() {
        let mut d = detail("v1.0", TagKind::Annotated);
        d.tagger = Some("Ada Lovelace <ada@example.com> 1753300000 +0000".to_string());
        let lines = tag_row_lines(&tag_row(&d));
        assert_eq!(lines.len(), 2, "tagger and the note — {lines:?}");
        assert_eq!(lines[1], TagRowLine::Absent(NO_MESSAGE));
        // Never both: a note *in place of* a message, not beside one.
        assert!(
            !lines.iter().any(|l| matches!(l, TagRowLine::Message(_))),
            "the note stands in for the message; showing both would be a \
             blank line followed by an explanation of it"
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
