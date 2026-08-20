//! Redaction (M1.09, #62): [`redact_operation`] keeps only an operation's
//! kind and never its free-text fields — the one property that lets this
//! module and [`crate::operations`] log a failing write without leaking a
//! commit message to stderr. Extracted verbatim from `durable.rs`'s inline
//! `mod tests` (a `#[cfg(test)]` child module) so the parent file can be read
//! as production code — see its module doc comment's "## Redaction" section
//! for the subsystem this exercises. A child module of `durable`, so it
//! still reaches `durable.rs`'s private items through `super::`. The journal
//! and recovery-ref tests that shared this `mod tests` block but exercise
//! different subsystems live separately in `journal_suite.rs` and
//! `recovery_ref_suite.rs`.

use super::*;
use git_vista_protocol::CommitMessage;

#[test]
fn redaction_keeps_the_operation_kind_and_never_its_free_text_fields() {
    let op = GitOperation::CommitOnHead {
        message: CommitMessage::new("a very private commit message").unwrap(),
        allow_empty: false,
    };
    let redacted = redact_operation(&op);
    assert_eq!(redacted, "commit_on_head");
    assert!(!redacted.contains("private"));
}
