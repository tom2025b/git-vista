//! Reading git's conflict marker file into blocks a person can choose between
//! (M4.31c, #432).
//!
//! # Why parse the marker file rather than compose from the three panes
//!
//! ADR 0069 decided the editor seeds from **the working-tree marker file** —
//! the same bytes `git merge` wrote and every terminal merge tool shows —
//! rather than composing text from the three stage blobs. Composing would
//! sidestep a real staleness gap (that file is invisible to the repository
//! generation), but at the cost of showing a document that can disagree with
//! what is actually on disk. The gap is closed by the `conflict-v1:` token
//! instead; this module is the other half of that decision.
//!
//! So: what git wrote is the source of truth here, and this module's only job
//! is to see the structure already in it.
//!
//! # Framework-free and host-tested, deliberately
//!
//! Same placement argument as `core.rs`, and it is load-bearing rather than
//! tidy: #432's acceptance criteria are facts about **what content a choice
//! produces**. Put that in the wasm viewer and `cargo test` never compiles it,
//! so the criteria would be pinned by nothing beside a green gate — the shape
//! of every defect in this repository's own record.
//!
//! # What this module refuses to do
//!
//! It does not decide whether a file is *eligible* for line-level resolution.
//! That is [`ConflictedFile::text_resolvable`], asked once, server-side and
//! client-side, so the two cannot drift. A caller reaching here has already
//! been told yes.

/// One run of the marker file: either text both sides agree on, or a conflict
/// with a side each.
///
/// An enum rather than a flag on a single struct because the two carry
/// genuinely different information — a context run has one body and no
/// decision to make; a conflict has two bodies and an open question. Collapsing
/// them would put an `Option` on every field and let a renderer forget which
/// case it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// Text outside any conflict. Shown, never chosen.
    Context { text: String },
    /// A conflicted run. `base` is `Some` only for a `diff3`-style marker file
    /// — git omits it under the default `merge` style, and **absent must not
    /// be rendered as empty**, the same distinction ADR 0063 draws for stages.
    Conflict {
        ours: String,
        theirs: String,
        base: Option<String>,
    },
}

/// Which side a conflict block is currently resolved to.
///
/// `Unchosen` is a real state, not a default to paper over: a file with any
/// unchosen block is not resolvable, and the UI must be able to say which
/// blocks are still open rather than silently picking one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Unchosen,
    Ours,
    Theirs,
    /// Both, ours first. Git offers no such resolution itself, but it is the
    /// common human answer to "these are two additions, keep them" — and the
    /// composed result is ordinary text, so nothing downstream needs to know.
    Both,
}

/// The `<<<<<<<`, `=======`, `>>>>>>>` and `|||||||` line prefixes git writes.
///
/// Matched as a **prefix on a line**, never anywhere in a line: a diff of a
/// merge tool's own documentation legitimately contains these strings mid-line,
/// and a substring match would split a file on its own prose.
const OURS_OPEN: &str = "<<<<<<<";
const BASE_SEP: &str = "|||||||";
const MID_SEP: &str = "=======";
const THEIRS_CLOSE: &str = ">>>>>>>";

/// Split a marker file into blocks.
///
/// **An unterminated conflict is returned as context, not as a conflict.** A
/// file whose `<<<<<<<` never reaches `>>>>>>>` is not a conflict this can
/// offer a choice about — the remaining text is whatever it is, and inventing a
/// `theirs` side from it would fabricate a version of the file that never
/// existed. Same refusal-to-guess posture as `Stage::Unreadable`.
pub fn parse(marker_text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut context = String::new();
    let mut lines = marker_text.split_inclusive('\n').peekable();

    while let Some(line) = lines.next() {
        if !starts_marker(line, OURS_OPEN) {
            context.push_str(line);
            continue;
        }

        // A conflict opens here. Everything gathered so far is context.
        let mut ours = String::new();
        let mut base: Option<String> = None;
        let mut theirs = String::new();
        let mut section = Section::Ours;
        let mut closed = false;
        // Kept so an unterminated conflict can be handed back verbatim rather
        // than reconstructed — reconstruction is where invention creeps in.
        let mut raw = line.to_string();

        for inner in lines.by_ref() {
            raw.push_str(inner);
            if starts_marker(inner, BASE_SEP) {
                base = Some(String::new());
                section = Section::Base;
            } else if starts_marker(inner, MID_SEP) {
                section = Section::Theirs;
            } else if starts_marker(inner, THEIRS_CLOSE) {
                closed = true;
                break;
            } else {
                match section {
                    Section::Ours => ours.push_str(inner),
                    Section::Base => {
                        base.get_or_insert_with(String::new).push_str(inner);
                    }
                    Section::Theirs => theirs.push_str(inner),
                }
            }
        }

        if closed {
            if !context.is_empty() {
                blocks.push(Block::Context {
                    text: std::mem::take(&mut context),
                });
            }
            blocks.push(Block::Conflict { ours, theirs, base });
        } else {
            // Unterminated: hand back exactly what was read.
            context.push_str(&raw);
        }
    }

    if !context.is_empty() {
        blocks.push(Block::Context { text: context });
    }
    blocks
}

enum Section {
    Ours,
    Base,
    Theirs,
}

/// True when `line` begins with `marker` followed by a space, a newline, or
/// nothing — git writes `<<<<<<< HEAD`, and a bare `<<<<<<<` is legal too.
fn starts_marker(line: &str, marker: &str) -> bool {
    let Some(rest) = line.strip_prefix(marker) else {
        return false;
    };
    rest.is_empty() || rest.starts_with([' ', '\n', '\r'])
}

/// Compose the resolved file from blocks and one choice per conflict.
///
/// `choices` is indexed by **conflict ordinal**, not by block index — a caller
/// iterating blocks to render them should not have to track which of them were
/// conflicts to look up a choice.
///
/// Returns `None` when any conflict is still [`Choice::Unchosen`]. That is the
/// point of the return type: there is no sensible file to produce, and
/// defaulting to one side would submit a resolution the user never made.
pub fn compose(blocks: &[Block], choices: &[Choice]) -> Option<String> {
    let mut out = String::new();
    let mut nth = 0usize;
    for block in blocks {
        match block {
            Block::Context { text } => out.push_str(text),
            Block::Conflict { ours, theirs, .. } => {
                let choice = choices.get(nth).copied().unwrap_or(Choice::Unchosen);
                nth += 1;
                match choice {
                    Choice::Unchosen => return None,
                    Choice::Ours => out.push_str(ours),
                    Choice::Theirs => out.push_str(theirs),
                    Choice::Both => {
                        out.push_str(ours);
                        out.push_str(theirs);
                    }
                }
            }
        }
    }
    Some(out)
}

/// How many conflicts the file holds — what a caller sizes its choice vector to.
pub fn conflict_count(blocks: &[Block]) -> usize {
    blocks
        .iter()
        .filter(|b| matches!(b, Block::Conflict { .. }))
        .count()
}

/// The ordinals of conflicts still awaiting a decision, for a UI that must say
/// which ones rather than only that some remain.
pub fn unchosen(blocks: &[Block], choices: &[Choice]) -> Vec<usize> {
    (0..conflict_count(blocks))
        .filter(|i| choices.get(*i).copied().unwrap_or(Choice::Unchosen) == Choice::Unchosen)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &str =
        "before\n<<<<<<< HEAD\nours line\n=======\ntheirs line\n>>>>>>> theirs\nafter\n";

    #[test]
    fn a_marker_file_splits_into_context_and_conflict() {
        let blocks = parse(SIMPLE);
        assert_eq!(blocks.len(), 3);
        assert_eq!(
            blocks[0],
            Block::Context {
                text: "before\n".into()
            }
        );
        assert_eq!(
            blocks[1],
            Block::Conflict {
                ours: "ours line\n".into(),
                theirs: "theirs line\n".into(),
                base: None,
            }
        );
        assert_eq!(
            blocks[2],
            Block::Context {
                text: "after\n".into()
            }
        );
    }

    #[test]
    fn choosing_a_side_reproduces_that_side_exactly_with_its_context() {
        // #432's first acceptance criterion: block-level choice between ours
        // and theirs.
        //
        // MUTATION: swap the Ours and Theirs arms in `compose`. The user picks
        // one side and silently gets the other — the worst possible failure of
        // a merge tool, and one no type check would catch.
        let blocks = parse(SIMPLE);
        assert_eq!(
            compose(&blocks, &[Choice::Ours]).unwrap(),
            "before\nours line\nafter\n"
        );
        assert_eq!(
            compose(&blocks, &[Choice::Theirs]).unwrap(),
            "before\ntheirs line\nafter\n"
        );
    }

    #[test]
    fn an_unchosen_conflict_produces_no_file_at_all() {
        // THE test in this module. MUTATION: default `Unchosen` to `Ours` (or
        // skip the block). The composer would then hand back a complete-looking
        // file for a decision the user never made, and the transport would
        // happily write it — a resolution invented on the user's behalf.
        let blocks = parse(SIMPLE);
        assert_eq!(compose(&blocks, &[Choice::Unchosen]), None);
        assert_eq!(compose(&blocks, &[]), None, "a missing choice is unchosen");
        assert_eq!(unchosen(&blocks, &[]), vec![0]);
        assert!(unchosen(&blocks, &[Choice::Ours]).is_empty());
    }

    #[test]
    fn both_keeps_ours_first_then_theirs() {
        let blocks = parse(SIMPLE);
        assert_eq!(
            compose(&blocks, &[Choice::Both]).unwrap(),
            "before\nours line\ntheirs line\nafter\n"
        );
    }

    #[test]
    fn a_diff3_marker_file_keeps_its_base_and_a_plain_one_reports_none() {
        // `base: None` and `base: Some("")` are different facts — the same
        // distinction ADR 0063 draws between an absent stage and an empty one.
        // MUTATION: default base to Some(String::new()). A plain merge-style
        // file would then claim a common ancestor that git never wrote.
        let diff3 =
            "<<<<<<< HEAD\nours\n||||||| base\nbase text\n=======\ntheirs\n>>>>>>> theirs\n";
        let blocks = parse(diff3);
        assert_eq!(
            blocks[0],
            Block::Conflict {
                ours: "ours\n".into(),
                theirs: "theirs\n".into(),
                base: Some("base text\n".into()),
            }
        );

        let plain = parse(SIMPLE);
        let Block::Conflict { base, .. } = &plain[1] else {
            panic!("expected a conflict");
        };
        assert_eq!(*base, None, "a merge-style file has no recorded ancestor");
    }

    #[test]
    fn an_unterminated_conflict_is_context_not_an_invented_choice() {
        // A truncated or hand-mangled file. MUTATION: treat the tail as
        // `theirs` and emit a Conflict. The UI would offer a choice between
        // "ours" and text that is not a version of anything, and picking it
        // would write a file that never existed on either side.
        let broken = "before\n<<<<<<< HEAD\nours only\nno closing marker\n";
        let blocks = parse(broken);
        assert_eq!(conflict_count(&blocks), 0, "no choice may be offered");
        let Block::Context { text } = &blocks[0] else {
            panic!("expected context, got {:?}", blocks[0]);
        };
        assert!(
            text.contains("<<<<<<< HEAD") && text.contains("no closing marker"),
            "the bytes must be handed back verbatim: {text:?}"
        );
    }

    #[test]
    fn marker_like_prose_mid_line_does_not_split_the_file() {
        // Documentation about merge conflicts is a real file. MUTATION: match
        // the markers anywhere in the line rather than as a prefix — this
        // module's own doc comment would then split itself.
        let prose = "the line <<<<<<< HEAD opens a conflict\nand ======= divides it\n";
        let blocks = parse(prose);
        assert_eq!(conflict_count(&blocks), 0);
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn several_conflicts_are_chosen_independently_by_ordinal() {
        // The multi-conflict case, and the reason `choices` is indexed by
        // conflict ordinal rather than block index.
        let two = "a\n<<<<<<< HEAD\nours1\n=======\ntheirs1\n>>>>>>> t\nb\n\
                   <<<<<<< HEAD\nours2\n=======\ntheirs2\n>>>>>>> t\nc\n";
        let blocks = parse(two);
        assert_eq!(conflict_count(&blocks), 2);
        assert_eq!(
            compose(&blocks, &[Choice::Ours, Choice::Theirs]).unwrap(),
            "a\nours1\nb\ntheirs2\nc\n"
        );
        assert_eq!(
            unchosen(&blocks, &[Choice::Ours]),
            vec![1],
            "the SECOND conflict is the one still open"
        );
    }

    #[test]
    fn a_file_with_no_conflict_round_trips_unchanged() {
        // The negative control. Without it, every assertion above could pass on
        // an implementation that mangled ordinary text.
        let plain = "just\nsome\nlines\n";
        let blocks = parse(plain);
        assert_eq!(conflict_count(&blocks), 0);
        assert_eq!(compose(&blocks, &[]).unwrap(), plain);
    }

    #[test]
    fn a_file_with_no_trailing_newline_keeps_that_fact() {
        // Git does not add one, and a composer that did would change the file
        // beyond the user's decision.
        let no_nl = "before\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> t\ntail";
        let blocks = parse(no_nl);
        assert_eq!(
            compose(&blocks, &[Choice::Ours]).unwrap(),
            "before\nours\ntail"
        );
    }
}
