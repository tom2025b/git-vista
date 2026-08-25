//! A catalogue of deliberately-broken repositories — which is also the
//! teaching material.
//!
//! # What this crate is
//!
//! Every repository shape git-vista's tests need, built once, by real `git`,
//! under a name that says what it is. Before #448 there were twenty separate
//! `seeded_repo()` implementations across eighteen files, two independent
//! builders of a conflicted repository, and a third conflict fixture in the
//! browser harness written in JavaScript because extending the second would
//! have broken a spec asserting an exact conflicted count. That is the
//! ordinary cost of duplicated fixtures, and it had already been paid.
//!
//! # Why each shape carries an essay
//!
//! Because the documentation *is* the teaching material.
//!
//! git-vista wants to explain git to people who find it baffling, and the
//! obvious way to do that — #93's proposal — was an isolated Git simulator with
//! trainers built on top. That was cut, for a reason worth restating: a
//! parallel fake Git is a second system to maintain, and it can happily teach
//! something the real product does not do. A lesson that drifts from the code
//! is worse than no lesson.
//!
//! A catalogue of *real* repositories, broken in *real* ways, has no such gap.
//! The `conflict_delete_modify()` a test asserts against is the same one a
//! lesson opens. If the explanation here stops matching what git puts on disk,
//! a test goes red — which is exactly the property a documentation file sitting
//! next to the code can never have.
//!
//! So the docs are not a chore bolted on afterwards. Each shape states **what
//! is wrong**, **what git actually put on disk**, and **why it matters**,
//! written for a reader who does not already know. If a shape cannot be
//! explained in plain words, it is not understood well enough to be a fixture.
//!
//! # One implementation, in Rust
//!
//! `std::process::Command` driving real `git` is the single implementation.
//! The browser harness shells out to the `gv-fixture` binary rather than
//! building repositories in JavaScript, because two implementations of "a
//! repository broken in shape X" is the drift problem one layer up — and drift
//! between a *teaching* fixture and a *test* fixture means the thing being
//! taught is not the thing the code handles. See `docs/adr/0074`.
//!
//! # Using it
//!
//! ```no_run
//! let (_dir, repo) = git_vista_fixtures::seeded();
//! // `repo` is a path to a real repository; `_dir` owns its lifetime and
//! // must stay in scope, because dropping it deletes the repository.
//! ```

pub mod browser;
pub mod git;

mod broken;
mod conflict;
mod content;
mod seeded;

pub use broken::{broken_head, unrunnable};
pub use conflict::{
    conflict_add_add, conflict_binary, conflict_delete_modify, conflict_modify_modify,
    sequence_mid_revert,
};
pub use content::{
    binary_blob, four_mode, on_disk_len, path_battery, pathological_content, write_rows,
    BIG_TEXT_APPEND, BIG_TEXT_BYTES, BINARY_SENTINEL,
};
pub use seeded::{empty, seeded, seeded_dated, seeded_files, Fixture, PINNED_DATE};
