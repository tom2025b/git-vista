//! `gv-scrollcast` (#325) — renders git-vista's Print Graph view as a paced
//! vertical-scroll time-lapse video.
//!
//! An offline, one-shot CLI, deliberately outside the served application: it
//! drives a headless browser, walks a tall image with a computed camera, and
//! shells out to ffmpeg. None of that belongs on an axum route, so this crate
//! touches no existing server surface at all.
//!
//! The shape of a run:
//!
//! ```text
//!   capture   print view -> one full-height PNG (height verified, never truncated)
//!   pacing    commit density -> speed curve -> timeline (pure, host-tested)
//!   chapters  merges/month boundaries -> chapters.txt + pivot callouts
//!   encode    timeline -> frames -> H.264/yuv420p MP4 in ./out/
//! ```
//!
//! Everything decidable lives in [`pacing`] and is host-tested; the modules
//! that touch Chromium and ffmpeg are the plumbing that carries those
//! decisions out. That split is this repo's standing rule, applied here.

mod pacing;

fn main() -> anyhow::Result<()> {
    // The CLI, capture, chapters and encode paths land next (#325). The pure
    // pacing core is already complete and under test, which is deliberately
    // the order this repo builds in: decide first, plumb second.
    eprintln!("gv-scrollcast: not yet wired up — see issue #325");
    Ok(())
}
