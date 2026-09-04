//! The fact-to-sentence mapping: the only place English enters Explain Mode.
//!
//! Everything below the viewer speaks in typed values — a
//! [`Precondition`], a [`RefChange`], a [`WorktreeEffect`]. This module is
//! where those become words, and it is the whole of what a second language
//! would replace. That is #92's acceptance criterion 4 ("translation is
//! possible") made structural rather than promised.
//!
//! ## Why this is exhaustive, with no wildcard
//!
//! Same discipline as the accessors it renders (ADR 0091). A `_ =>` arm is how
//! a newly added fact kind acquires a wrong sentence — or a blank one —
//! silently. An inexhaustive match is a compile error, so adding a fact kind
//! stops the build until somebody writes its sentence.
//!
//! A wildcard is not the only way to ship a blank, though: an arm returning
//! `""` compiles and reads as done. [`tests::every_fact_kind_says_something`]
//! is the guard for that, and it walks a corpus holding **every variant of
//! every enum a fact can carry**, not one instance per fact kind.

use git_vista_protocol::{
    Advisory, CommitOid, Explanation, ExplanationFact, IndexEffect, NetworkNeed, Precondition,
    RecoveryStrategy, RefChange, RefState, RiskLevel, Topic, WorktreeEffect,
};

/// The visual object a line points at, so the viewer can link it — #92's
/// acceptance criterion 3.
///
/// Deliberately only the two the application already draws: a ref the graph
/// labels, and a commit with a dot. **No new glossary subsystem**, which
/// section 7 of the design names as out of scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// A ref the graph already draws.
    Ref(String),
    /// A commit that has a dot on the graph.
    Commit(String),
}

/// One rendered line: its sentence, and the object it points at if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub link: Option<LinkTarget>,
}

/// One rendered section, ready to collapse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSection {
    pub topic: Topic,
    pub heading: &'static str,
    /// Empty exactly when the underlying section carried no facts; the viewer
    /// then shows [`Self::when_empty`] rather than hiding the section.
    pub lines: Vec<Line>,
    /// What to say when `lines` is empty. Always populated, so a renderer can
    /// never reach for a `None` and print nothing.
    pub when_empty: &'static str,
}

/// Render a whole explanation, section by section, in the order
/// [`git_vista_protocol::explain`] fixed.
///
/// Sections with no facts are kept, not dropped: *"nothing must be true
/// first"* is itself the teaching sentence, and a panel whose shape changes
/// between operations makes a reader re-find every heading. See ADR 0091,
/// decision 5.
pub fn render(explanation: &Explanation) -> Vec<RenderedSection> {
    explanation
        .sections
        .iter()
        .map(|section| RenderedSection {
            topic: section.topic,
            heading: heading(section.topic),
            lines: section
                .facts
                .iter()
                .map(|fact| Line {
                    text: sentence(fact),
                    link: link_target(fact),
                })
                .collect(),
            when_empty: when_empty(section.topic),
        })
        .collect()
}

/// The heading for a topic. Plain words, not the enum's name — a reader who
/// has never heard of an "index" still has to know what the section is about.
pub fn heading(topic: Topic) -> &'static str {
    match topic {
        Topic::MustBeTrueFirst => "What must be true first",
        Topic::WhatMoves => "What moves",
        Topic::IndexAndWorktree => "Your files and staging area",
        Topic::Remote => "The network",
        Topic::HowToUndo => "How to get back",
        Topic::WorthKnowing => "Worth knowing",
    }
}

/// What a section says when it carries no facts.
///
/// Four of the six can never be empty in practice — files/staging always
/// carries two facts, the network and recovery one each, and "worth knowing"
/// always leads with the risk level because `Plan::risk` is a plain field
/// rather than an `Option`. They still get honest strings rather than a
/// `todo!()`, because a panic in a rendering path is a worse answer than a
/// sentence nobody sees — and because "can never be empty" is a claim about
/// [`git_vista_protocol::explain`], pinned over the whole operation
/// vocabulary by that crate's `every_operation_gets_all_six_sections_in_order`
/// and `worth_knowing_is_never_empty`, not by anything this module can see.
pub fn when_empty(topic: Topic) -> &'static str {
    match topic {
        Topic::MustBeTrueFirst => {
            "Nothing has to be true first — this runs against the repository as it stands."
        }
        Topic::WhatMoves => "No branch, tag or other ref moves.",
        Topic::IndexAndWorktree => "Nothing is known about the effect on your files.",
        Topic::Remote => "Nothing is known about whether this reaches the network.",
        Topic::HowToUndo => "Nothing is known about how to undo this.",
        Topic::WorthKnowing => "Nothing else worth flagging.",
    }
}

/// The `localStorage` key a section's collapsed state persists under.
///
/// **Keyed on the topic alone — never on the plan, the operation, or the
/// branch.** That is the whole design of it: an expert who collapses "What
/// must be true first" means *always*, and a key carrying anything
/// plan-shaped would make them collapse the same section again on every
/// operation, forever. The panel would look like it remembered while
/// remembering nothing that matters.
///
/// Six keys exist and no more, which is also why this returns a `&'static
/// str` rather than building a string: there is nothing to interpolate, and a
/// signature that cannot interpolate cannot drift into keying on a plan.
pub fn storage_key(topic: Topic) -> &'static str {
    match topic {
        Topic::MustBeTrueFirst => "git-vista.explain.must-be-true-first",
        Topic::WhatMoves => "git-vista.explain.what-moves",
        Topic::IndexAndWorktree => "git-vista.explain.index-and-worktree",
        Topic::Remote => "git-vista.explain.remote",
        Topic::HowToUndo => "git-vista.explain.how-to-undo",
        Topic::WorthKnowing => "git-vista.explain.worth-knowing",
    }
}

/// A run of a sentence, split on the backticks that mark git's own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    /// Ordinary prose.
    Text(String),
    /// A ref name, an oid, a remote — something the repository itself named,
    /// which the view sets in monospace so it is not mistaken for English.
    Code(String),
}

/// Split a sentence into prose and code runs.
///
/// The sentences above mark git's own words with backticks, because a branch
/// called `main` and the English word "main" are not the same thing and a
/// reader has to be able to tell. Nothing renders markdown here — the modal is
/// inline-styled plain DOM — so this is the whole of the formatting the panel
/// supports, deliberately: one rule, no nesting, no escapes to get wrong.
///
/// An unclosed backtick yields the rest of the sentence as prose rather than
/// swallowing it. A sentence is never worth losing to a typo in its own
/// punctuation.
pub fn spans(text: &str) -> Vec<Span> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let (before, after_open) = rest.split_at(open);
        if !before.is_empty() {
            out.push(Span::Text(before.to_string()));
        }
        let after_open = &after_open[1..];
        match after_open.find('`') {
            Some(close) => {
                out.push(Span::Code(after_open[..close].to_string()));
                rest = &after_open[close + 1..];
            }
            // Unclosed: the backtick was a typo, not a marker. Keep the words.
            None => {
                out.push(Span::Text(format!("`{after_open}")));
                return out;
            }
        }
    }
    if !rest.is_empty() {
        out.push(Span::Text(rest.to_string()));
    }
    out
}

/// One fact, in words. Exhaustive over every fact kind and every variant each
/// one can carry.
pub fn sentence(fact: &ExplanationFact) -> String {
    match fact {
        ExplanationFact::Precondition(p) => precondition(p),
        ExplanationFact::RefMoves(r) => ref_change(r),
        ExplanationFact::Worktree(w) => worktree(*w).to_string(),
        ExplanationFact::Index(i) => index(*i).to_string(),
        ExplanationFact::Remote(n) => remote(*n).to_string(),
        ExplanationFact::Recovery(r) => recovery(r),
        ExplanationFact::Advisory(a) => advisory(a),
        ExplanationFact::Risk(l) => risk(*l).to_string(),
    }
}

/// The visual object this line points at, if the application draws one.
///
/// A fact naming a ref links to that ref; one naming only a commit links to
/// its dot. A fact naming neither — every effect, every risk level — links to
/// nothing, and says so rather than inventing an anchor.
pub fn link_target(fact: &ExplanationFact) -> Option<LinkTarget> {
    match fact {
        ExplanationFact::Precondition(p) => match p {
            Precondition::RefAt { ref_name, .. }
            | Precondition::RefExists { ref_name }
            | Precondition::RefAbsent { ref_name } => Some(LinkTarget::Ref(ref_name.to_string())),
            // A branch name is a ref the graph draws, under its full name.
            // The collision precondition (M11.02, #547) names a branch too —
            // and links to *that* branch, not to the worktree holding it: the
            // graph draws refs, and there is no worktree object on it to
            // point at.
            Precondition::BranchCheckedOut { branch }
            | Precondition::BranchNotCheckedOut { branch }
            | Precondition::BranchFreeInEveryOtherWorktree { branch } => {
                Some(LinkTarget::Ref(format!("refs/heads/{branch}")))
            }
            // Neither names anything the graph draws. A clean worktree is a
            // state, not an object, and the seed is not in this repository's
            // history at all.
            Precondition::CleanWorktree | Precondition::SeedRecorded => None,
            Precondition::RemoteConfigured { .. } => None,
        },
        ExplanationFact::RefMoves(r) => Some(LinkTarget::Ref(r.ref_name.to_string())),
        ExplanationFact::Recovery(r) => match r {
            RecoveryStrategy::ResetRef { ref_name, .. } => {
                Some(LinkTarget::Ref(ref_name.to_string()))
            }
            // The branch or tag does not exist yet, so the commit is the only
            // thing on screen to point at.
            RecoveryStrategy::RecreateBranch { at, .. }
            | RecoveryStrategy::RecreateTag { at, .. }
            | RecoveryStrategy::RecreateStashEntry { at, .. } => {
                Some(LinkTarget::Commit(at.to_string()))
            }
            RecoveryStrategy::DeleteCreatedBranch { name } => {
                Some(LinkTarget::Ref(format!("refs/heads/{name}")))
            }
            RecoveryStrategy::DeleteCreatedTag { name } => {
                Some(LinkTarget::Ref(format!("refs/tags/{name}")))
            }
            RecoveryStrategy::CheckoutPrevious { branch } => {
                Some(LinkTarget::Ref(format!("refs/heads/{branch}")))
            }
            RecoveryStrategy::RevertCommit { commit } => {
                Some(LinkTarget::Commit(commit.to_string()))
            }
            RecoveryStrategy::NotNeeded
            | RecoveryStrategy::RecoverableIfStaged
            | RecoveryStrategy::ConflictRecreatableWhileInProgress
            | RecoveryStrategy::Irrecoverable => None,
        },
        ExplanationFact::Advisory(a) => match a {
            Advisory::DefaultBranchPush { branch, .. }
            | Advisory::RemoteHistoryReplaced { branch, .. } => {
                Some(LinkTarget::Ref(format!("refs/heads/{branch}")))
            }
            // The whole content of this advisory is that the answer is not
            // known. Linking somewhere would suggest it is.
            Advisory::DefaultBranchUnknown { .. } => None,
        },
        // Effects and risk describe the operation, not an object in it.
        ExplanationFact::Worktree(_)
        | ExplanationFact::Index(_)
        | ExplanationFact::Remote(_)
        | ExplanationFact::Risk(_) => None,
    }
}

// ---------------------------------------------------------------------------
// The tables
// ---------------------------------------------------------------------------

fn precondition(p: &Precondition) -> String {
    match p {
        Precondition::RefAt { ref_name, oid } => format!(
            "`{ref_name}` must still be at `{}`. If it has moved since this \
             preview, the operation is refused rather than run against a \
             repository that changed underneath it.",
            short(oid)
        ),
        Precondition::RefExists { ref_name } => format!("`{ref_name}` must exist."),
        Precondition::RefAbsent { ref_name } => {
            format!("`{ref_name}` must not exist yet — nothing is overwritten.")
        }
        Precondition::BranchCheckedOut { branch } => {
            format!("`{branch}` must be the branch you currently have checked out.")
        }
        Precondition::BranchNotCheckedOut { branch } => {
            format!(
                "`{branch}` must be some branch OTHER than the one you currently have checked out."
            )
        }
        Precondition::CleanWorktree => {
            "Your working tree must have no uncommitted changes — nothing edited, nothing staged."
                .to_string()
        }
        Precondition::RemoteConfigured { remote } => {
            format!("A remote named `{remote}` must be configured.")
        }
        Precondition::SeedRecorded => {
            "The demo repository's seed must be on record, so it can be rebuilt from it."
                .to_string()
        }
        // M11.02 (#547). The sentence cannot name the holding worktree — the
        // precondition carries the branch and nothing else, because *which*
        // worktree holds it is an observation that can change between this
        // preview and the moment the operation runs. So it teaches the rule
        // and points at the one command that answers "which one?", which is
        // what a learner can act on. The refusal, when it happens, does name
        // the worktree: the server has the census in hand there.
        Precondition::BranchFreeInEveryOtherWorktree { branch } => {
            format!(
                "`{branch}` must not be checked out in any OTHER worktree of this \
                 repository. Git allows a branch in only one working tree at a time — \
                 two would let the same branch move from two directions at once — so \
                 if another worktree has it, this is refused instead of attempted. \
                 `git worktree list` shows which worktree holds what."
            )
        }
    }
}

/// A ref change, as a sentence rather than an arrow.
///
/// `main: abc → def` is compact and assumes the reader already knows what a
/// ref is. Creation and deletion in particular deserve their own wording:
/// "moves from does not exist to `abc1234`" is not English, and a deletion is
/// the one case where the *before* value is the fact worth reading, because it
/// is what you would need to put it back.
fn ref_change(r: &RefChange) -> String {
    match (&r.before, &r.after) {
        // Not reachable from any plan the server builds — a change that
        // changes nothing would not be listed. Stated rather than
        // `unreachable!()`: a panic in a rendering path is the worst possible
        // answer to a surprising plan.
        (RefState::Absent, RefState::Absent) => {
            format!("`{}` does not exist, and still will not.", r.ref_name)
        }
        (RefState::Absent, after) => {
            format!("`{}` is created, at {}.", r.ref_name, ref_state(after))
        }
        (before, RefState::Absent) => format!(
            "`{}` is deleted. It is at {} right now, which is what you would \
             need to put it back.",
            r.ref_name,
            ref_state(before)
        ),
        (before, after) => format!(
            "`{}` moves from {} to {}.",
            r.ref_name,
            ref_state(before),
            ref_state(after)
        ),
    }
}

fn ref_state(s: &RefState) -> String {
    match s {
        // Only reached if [`ref_change`]'s first two arms are ever bypassed;
        // they handle every absent case with wording that reads.
        RefState::Absent => "nothing".to_string(),
        RefState::At(oid) => format!("`{}`", short(oid)),
        RefState::Symbolic(name) => format!("`{name}`"),
        // The plan cannot name this oid because the commit does not exist
        // yet — saying so is more honest than an ellipsis.
        RefState::Computed => "a new commit this operation creates".to_string(),
    }
}

fn worktree(w: WorktreeEffect) -> &'static str {
    match w {
        WorktreeEffect::Untouched => "No file in your working tree changes.",
        WorktreeEffect::FilesRewritten => "Tracked files are rewritten in place.",
        WorktreeEffect::FilesRemoved => "Files are removed from your working tree.",
        WorktreeEffect::MayConflict => {
            "Tracked files are rewritten — and this can stop part-way, leaving \
             conflict markers in them for you to finish."
        }
        WorktreeEffect::RewrittenIfCheckedOut => {
            "If this is the branch you have checked out, your files are \
             rewritten. If it is not, nothing in your working tree changes."
        }
    }
}

fn index(i: IndexEffect) -> &'static str {
    match i {
        IndexEffect::Untouched => "The staging area is not touched.",
        IndexEffect::EntriesStaged => "Paths move from unstaged to staged.",
        IndexEffect::EntriesUnstaged => "Paths move from staged to unstaged.",
        IndexEffect::StagesResolved => {
            "The conflicting versions of the path collapse into one resolved entry."
        }
        IndexEffect::Rebuilt => {
            "The staging area is set from what this operation produces, so what \
             you had staged is not what will be there afterwards."
        }
        IndexEffect::MayGainConflictStages => {
            "The staging area is left exactly as it is — unless this stops on a \
             conflict, which writes the conflicting versions into it."
        }
        IndexEffect::RebuiltIfCheckedOut => {
            "If this is the branch you have checked out, the staging area is set \
             from the result. If it is not, it is not touched."
        }
    }
}

fn remote(n: NetworkNeed) -> &'static str {
    match n {
        NetworkNeed::Remote => "This reaches the remote over the network.",
        NetworkNeed::Local => {
            "This stays inside your repository. Nothing is sent anywhere, and it \
             works with the network off."
        }
    }
}

fn recovery(r: &RecoveryStrategy) -> String {
    match r {
        RecoveryStrategy::NotNeeded => {
            "Nothing to undo — this changes nothing that needs undoing.".to_string()
        }
        RecoveryStrategy::ResetRef { ref_name, to } => {
            format!("To undo: move `{ref_name}` back to `{}`.", short(to))
        }
        RecoveryStrategy::RecreateBranch { name, at } => {
            format!("To undo: recreate the branch `{name}` at `{}`.", short(at))
        }
        RecoveryStrategy::DeleteCreatedBranch { name } => {
            format!("To undo: delete the branch `{name}` this creates.")
        }
        RecoveryStrategy::RecreateTag { name, at } => {
            format!("To undo: recreate the tag `{name}` at `{}`.", short(at))
        }
        RecoveryStrategy::RecreateStashEntry { at, message } => match message {
            Some(m) => format!(
                "To undo: recreate the stash entry — `{}`, saved as \"{m}\".",
                short(at)
            ),
            None => format!("To undo: recreate the stash entry at `{}`.", short(at)),
        },
        RecoveryStrategy::DeleteCreatedTag { name } => {
            format!("To undo: delete the tag `{name}` this creates.")
        }
        RecoveryStrategy::CheckoutPrevious { branch } => {
            format!("To undo: check `{branch}` back out.")
        }
        RecoveryStrategy::RevertCommit { commit } => {
            format!(
                "To undo: revert `{}` — a new commit that takes the change back \
                 out, leaving the original in history.",
                short(commit)
            )
        }
        // The nuance matters and is stated rather than softened: the strategy
        // tag alone would read as more recoverable than it is.
        RecoveryStrategy::RecoverableIfStaged => {
            "Recoverable only if the content had been staged: its copy is still \
             in git's object store until the next cleanup. Content that was \
             never staged has no copy left anywhere."
                .to_string()
        }
        RecoveryStrategy::ConflictRecreatableWhileInProgress => {
            "The conflict can be brought back while this sequence is still in \
             progress. Once it finishes, it cannot."
                .to_string()
        }
        RecoveryStrategy::Irrecoverable => {
            "Git-Vista offers no undo for this — no ref moves back, and nothing in \
             its own record can replay it."
                .to_string()
        }
    }
}

fn advisory(a: &Advisory) -> String {
    match a {
        Advisory::DefaultBranchPush { branch, remote } => format!(
            "`{branch}` is `{remote}`'s default branch. Legal, and often exactly \
             what you meant — worth seeing, because the cost of getting it \
             wrong is everyone's."
        ),
        // The whole content is the gap, so it is stated as one. This is the
        // distinction between "I checked" and "I could not check".
        Advisory::DefaultBranchUnknown { reason } => format!(
            "Whether this targets the default branch could NOT be determined \
             ({reason}). Not an error and not a refusal — a stated gap in \
             what this preview can tell you."
        ),
        Advisory::RemoteHistoryReplaced { branch, remote } => format!(
            "If this succeeds, `{branch}` on `{remote}` is replaced, and nothing \
             this application offers can put it back there. Recovery below \
             describes what can be restored on your own machine; this is the part it \
             cannot reach."
        ),
    }
}

fn risk(l: RiskLevel) -> &'static str {
    match l {
        RiskLevel::Safe => "Safe — nothing you have locally can be lost.",
        RiskLevel::Reversible => "Reversible — there is a stated way back, in this same panel.",
        RiskLevel::Destructive => {
            "Destructive — something goes away that no ref names afterwards. \
             Read the undo line before confirming."
        }
        RiskLevel::Remote => {
            "Remote — this reaches past your machine to a server someone else may be reading."
        }
    }
}

/// The first seven characters of an oid, as git itself abbreviates.
///
/// Not truncation for its own sake: a forty-character hex string in the middle
/// of a sentence is unreadable, and the full value is still on the typed fact
/// for anything that needs it.
fn short(oid: &CommitOid) -> String {
    oid.to_string().chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_vista_protocol::{BranchName, RefName, RemoteName, StashMessage, TagName};

    fn oid(c: char) -> CommitOid {
        CommitOid::new(c.to_string().repeat(40)).unwrap()
    }
    fn rname(s: &str) -> RefName {
        RefName::new(s).unwrap()
    }
    fn branch(s: &str) -> BranchName {
        BranchName::new(s).unwrap()
    }

    /// **Every variant of every enum a fact can carry** — not one instance per
    /// fact kind. A corpus that held one `Precondition` would let seven
    /// precondition arms return `""` and still pass.
    fn every_fact() -> Vec<ExplanationFact> {
        use ExplanationFact as F;
        let mut v = vec![
            F::Precondition(Precondition::RefAt {
                ref_name: rname("refs/heads/main"),
                oid: oid('a'),
            }),
            F::Precondition(Precondition::RefExists {
                ref_name: rname("refs/heads/main"),
            }),
            F::Precondition(Precondition::RefAbsent {
                ref_name: rname("refs/heads/new"),
            }),
            F::Precondition(Precondition::BranchCheckedOut {
                branch: branch("main"),
            }),
            F::Precondition(Precondition::BranchNotCheckedOut {
                branch: branch("main"),
            }),
            F::Precondition(Precondition::CleanWorktree),
            F::Precondition(Precondition::RemoteConfigured {
                remote: RemoteName::new("origin").unwrap(),
            }),
            F::Precondition(Precondition::SeedRecorded),
            F::Precondition(Precondition::BranchFreeInEveryOtherWorktree {
                branch: branch("feature/x"),
            }),
            F::Recovery(RecoveryStrategy::NotNeeded),
            F::Recovery(RecoveryStrategy::ResetRef {
                ref_name: rname("refs/heads/main"),
                to: oid('b'),
            }),
            F::Recovery(RecoveryStrategy::RecreateBranch {
                name: branch("gone"),
                at: oid('c'),
            }),
            F::Recovery(RecoveryStrategy::DeleteCreatedBranch {
                name: branch("fresh"),
            }),
            F::Recovery(RecoveryStrategy::RecreateTag {
                name: TagName::new("v1.0").unwrap(),
                at: oid('d'),
            }),
            F::Recovery(RecoveryStrategy::RecreateStashEntry {
                at: oid('e'),
                message: Some(StashMessage::new("wip").unwrap()),
            }),
            F::Recovery(RecoveryStrategy::RecreateStashEntry {
                at: oid('e'),
                message: None,
            }),
            F::Recovery(RecoveryStrategy::DeleteCreatedTag {
                name: TagName::new("v2.0").unwrap(),
            }),
            F::Recovery(RecoveryStrategy::CheckoutPrevious {
                branch: branch("main"),
            }),
            F::Recovery(RecoveryStrategy::RevertCommit { commit: oid('f') }),
            F::Recovery(RecoveryStrategy::RecoverableIfStaged),
            F::Recovery(RecoveryStrategy::ConflictRecreatableWhileInProgress),
            F::Recovery(RecoveryStrategy::Irrecoverable),
            F::Advisory(Advisory::DefaultBranchPush {
                branch: branch("main"),
                remote: RemoteName::new("origin").unwrap(),
            }),
            F::Advisory(Advisory::DefaultBranchUnknown {
                reason: "no refs/remotes/origin/HEAD".to_string(),
            }),
            F::Advisory(Advisory::RemoteHistoryReplaced {
                branch: branch("main"),
                remote: RemoteName::new("origin").unwrap(),
            }),
        ];
        // Every RefState, on both sides of a change.
        for before in [
            RefState::Absent,
            RefState::At(oid('1')),
            RefState::Symbolic(rname("refs/heads/main")),
            RefState::Computed,
        ] {
            v.push(F::RefMoves(RefChange {
                ref_name: rname("refs/heads/main"),
                before,
                after: RefState::Computed,
            }));
        }
        for w in [
            WorktreeEffect::Untouched,
            WorktreeEffect::FilesRewritten,
            WorktreeEffect::FilesRemoved,
            WorktreeEffect::MayConflict,
            WorktreeEffect::RewrittenIfCheckedOut,
        ] {
            v.push(F::Worktree(w));
        }
        for i in [
            IndexEffect::Untouched,
            IndexEffect::EntriesStaged,
            IndexEffect::EntriesUnstaged,
            IndexEffect::StagesResolved,
            IndexEffect::Rebuilt,
            IndexEffect::MayGainConflictStages,
            IndexEffect::RebuiltIfCheckedOut,
        ] {
            v.push(F::Index(i));
        }
        for n in [NetworkNeed::Remote, NetworkNeed::Local] {
            v.push(F::Remote(n));
        }
        for l in [
            RiskLevel::Safe,
            RiskLevel::Reversible,
            RiskLevel::Destructive,
            RiskLevel::Remote,
        ] {
            v.push(F::Risk(l));
        }
        v
    }

    #[test]
    fn every_fact_kind_says_something() {
        // The guard against a stub arm. Exhaustiveness is a compile-time
        // check that every arm EXISTS; this is the run-time check that every
        // arm SAYS something, which no compiler can make.
        for fact in every_fact() {
            let s = sentence(&fact);
            assert!(!s.trim().is_empty(), "{fact:?} renders as nothing");
            assert!(
                s.len() > 12,
                "{fact:?} renders as {s:?} — too short to be a sentence"
            );
            assert!(
                s.ends_with('.'),
                "{fact:?} renders as {s:?} — not a finished sentence"
            );
        }
    }

    #[test]
    fn the_corpus_is_not_quietly_thin() {
        // If a variant is added to any of these enums and nobody adds it to
        // `every_fact`, the test above keeps passing over a corpus that no
        // longer covers the vocabulary. These counts are the tripwire.
        let facts = every_fact();
        let count = |f: fn(&ExplanationFact) -> bool| facts.iter().filter(|x| f(x)).count();
        assert_eq!(
            count(|f| matches!(f, ExplanationFact::Precondition(_))),
            9,
            "Precondition has 9 variants"
        );
        assert_eq!(
            count(|f| matches!(f, ExplanationFact::Recovery(_))),
            13,
            "RecoveryStrategy has 12 variants, one of them tested both ways"
        );
        assert_eq!(
            count(|f| matches!(f, ExplanationFact::Advisory(_))),
            3,
            "Advisory has 3 variants"
        );
        assert_eq!(
            count(|f| matches!(f, ExplanationFact::Worktree(_))),
            5,
            "WorktreeEffect has 5 variants"
        );
        assert_eq!(
            count(|f| matches!(f, ExplanationFact::Index(_))),
            7,
            "IndexEffect has 7 variants"
        );
        assert_eq!(
            count(|f| matches!(f, ExplanationFact::Remote(_))),
            2,
            "NetworkNeed has 2 variants"
        );
        assert_eq!(
            count(|f| matches!(f, ExplanationFact::Risk(_))),
            4,
            "RiskLevel has 4 variants"
        );
        assert_eq!(
            count(|f| matches!(f, ExplanationFact::RefMoves(_))),
            4,
            "RefState has 4 variants, each exercised as a `before`"
        );
    }

    #[test]
    fn no_two_facts_render_identically() {
        // A copy-paste in a table this size is invisible on review and reads
        // as a correct sentence about the wrong thing. Two distinct typed
        // facts saying the same words is the shape that catches it.
        let facts = every_fact();
        for (i, a) in facts.iter().enumerate() {
            for b in facts.iter().skip(i + 1) {
                assert_ne!(
                    sentence(a),
                    sentence(b),
                    "{a:?} and {b:?} render to the same sentence"
                );
            }
        }
    }

    #[test]
    fn a_fact_that_names_a_ref_links_to_it() {
        // Criterion 3, in the direction that can actually be wrong: a link
        // must point at the ref the fact names, not merely exist.
        let f = ExplanationFact::RefMoves(RefChange {
            ref_name: rname("refs/heads/feature/idea"),
            before: RefState::Absent,
            after: RefState::At(oid('1')),
        });
        assert_eq!(
            link_target(&f),
            Some(LinkTarget::Ref("refs/heads/feature/idea".to_string()))
        );

        let f = ExplanationFact::Precondition(Precondition::BranchCheckedOut {
            branch: branch("main"),
        });
        assert_eq!(
            link_target(&f),
            Some(LinkTarget::Ref("refs/heads/main".to_string()))
        );

        let f = ExplanationFact::Recovery(RecoveryStrategy::RevertCommit { commit: oid('a') });
        assert_eq!(link_target(&f), Some(LinkTarget::Commit("a".repeat(40))));
    }

    #[test]
    fn a_fact_that_names_nothing_links_to_nothing() {
        // The other half, and the one that matters more: an effect describes
        // the operation, not an object. Inventing an anchor for it would make
        // the panel offer a link that goes somewhere unrelated.
        for f in [
            ExplanationFact::Worktree(WorktreeEffect::MayConflict),
            ExplanationFact::Index(IndexEffect::Rebuilt),
            ExplanationFact::Remote(NetworkNeed::Local),
            ExplanationFact::Risk(RiskLevel::Destructive),
            ExplanationFact::Precondition(Precondition::CleanWorktree),
            ExplanationFact::Recovery(RecoveryStrategy::Irrecoverable),
            ExplanationFact::Advisory(Advisory::DefaultBranchUnknown {
                reason: "unreadable".to_string(),
            }),
        ] {
            assert_eq!(link_target(&f), None, "{f:?} should link to nothing");
        }
    }

    #[test]
    fn an_unknown_advisory_never_links_anywhere() {
        // Called out on its own because it is the tempting one to get wrong:
        // the advisory mentions a push, so a link feels natural. Its whole
        // content is that the answer is NOT known, and a link would suggest
        // otherwise.
        let f = ExplanationFact::Advisory(Advisory::DefaultBranchUnknown {
            reason: "no refs/remotes/origin/HEAD".to_string(),
        });
        assert_eq!(link_target(&f), None);
        assert!(
            sentence(&f).contains("could NOT be"),
            "the sentence must state the gap, not describe a push"
        );
    }

    #[test]
    fn every_topic_has_a_heading_and_an_empty_line() {
        for t in [
            Topic::MustBeTrueFirst,
            Topic::WhatMoves,
            Topic::IndexAndWorktree,
            Topic::Remote,
            Topic::HowToUndo,
            Topic::WorthKnowing,
        ] {
            assert!(!heading(t).trim().is_empty(), "{t:?} has no heading");
            assert!(!when_empty(t).trim().is_empty(), "{t:?} has no empty line");
        }
    }

    #[test]
    fn spans_split_on_backticks_and_lose_nothing() {
        assert_eq!(
            spans("`main` moves from `abc1234` to a new commit."),
            vec![
                Span::Code("main".into()),
                Span::Text(" moves from ".into()),
                Span::Code("abc1234".into()),
                Span::Text(" to a new commit.".into()),
            ]
        );
        assert_eq!(
            spans("No file in your working tree changes."),
            vec![Span::Text("No file in your working tree changes.".into())]
        );
        // The property that matters more than the shape: every character of
        // the sentence survives the split. A renderer that silently drops
        // words is worse than one that shows a stray backtick.
        for fact in every_fact() {
            let s = sentence(&fact);
            let rebuilt: String = spans(&s)
                .into_iter()
                .map(|sp| match sp {
                    Span::Text(t) => t,
                    Span::Code(c) => format!("`{c}`"),
                })
                .collect();
            assert_eq!(rebuilt, s, "spans lost characters from {s:?}");
        }
    }

    #[test]
    fn an_unclosed_backtick_keeps_its_words() {
        assert_eq!(
            spans("a `broken sentence"),
            vec![
                Span::Text("a ".into()),
                Span::Text("`broken sentence".into()),
            ]
        );
    }

    #[test]
    fn every_topic_has_its_own_storage_key_and_they_are_all_distinct() {
        let topics = [
            Topic::MustBeTrueFirst,
            Topic::WhatMoves,
            Topic::IndexAndWorktree,
            Topic::Remote,
            Topic::HowToUndo,
            Topic::WorthKnowing,
        ];
        let mut keys: Vec<&str> = topics.iter().map(|t| storage_key(*t)).collect();
        keys.sort();
        let distinct = {
            let mut d = keys.clone();
            d.dedup();
            d
        };
        assert_eq!(
            keys, distinct,
            "two topics share a storage key — collapsing one would collapse the other"
        );
        for k in &keys {
            assert!(
                k.starts_with("git-vista.explain."),
                "{k:?} is not namespaced with the rest of this app's preferences"
            );
        }
    }

    #[test]
    fn a_storage_key_carries_nothing_but_its_topic() {
        // The defect this guards against does not look like a bug: a key of
        // "git-vista.explain.what-moves.refs/heads/main" persists perfectly
        // well and reads as thorough. It just means an expert who collapses a
        // section collapses it for *that branch*, and has to do it again on
        // the next one, forever. The signature returns `&'static str`
        // precisely so there is nothing to interpolate — this asserts the
        // resulting keys are in fact constant-shaped.
        for t in [
            Topic::MustBeTrueFirst,
            Topic::WhatMoves,
            Topic::IndexAndWorktree,
            Topic::Remote,
            Topic::HowToUndo,
            Topic::WorthKnowing,
        ] {
            let k = storage_key(t);
            assert_eq!(
                k.matches('.').count(),
                2,
                "{k:?} has more segments than `git-vista.explain.<topic>` — \
                 something plan-shaped may have crept into the key"
            );
            assert!(
                !k.contains('/') && !k.contains(':'),
                "{k:?} looks like it carries a ref or an oid"
            );
        }
    }

    #[test]
    fn headings_are_plain_words_not_type_names() {
        // The panel is for someone who has never heard the word "index".
        for t in [
            Topic::MustBeTrueFirst,
            Topic::WhatMoves,
            Topic::IndexAndWorktree,
            Topic::Remote,
            Topic::HowToUndo,
            Topic::WorthKnowing,
        ] {
            let h = heading(t);
            assert!(
                !h.contains("Effect") && !h.contains("Topic") && !h.contains("::"),
                "{t:?}'s heading {h:?} reads like a type name"
            );
        }
    }
}
