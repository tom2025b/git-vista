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
//!   encode    timeline -> frames -> H.264/yuv420p MP4 in <out>/
//! ```
//!
//! Everything decidable lives in [`pacing`] and is host-tested; the modules
//! that touch Chromium and ffmpeg are the plumbing that carries those
//! decisions out. That split is this repo's standing rule, applied here.
//!
//! # Three honest gaps in this wiring — read before assuming a flag "works"
//!
//! This lane (CLI + docs) wires three already-built, independently-tested
//! modules together. Two of the owner's requested flags cannot be fully
//! honoured from what those modules currently expose, and a third required
//! working around a hardcoded path rather than editing a file outside this
//! lane's mandate. Each is explained where it's implemented below; this list
//! is the map so a reader doesn't have to hunt for them:
//!
//! 1. **Pivot callouts never fire yet** (see `run_pipeline`'s `chapters::
//!    detect_pivots` call site for the full accounting). `detect_pivots`
//!    needs `&[git_vista_core::model::GraphRow]` — a real `Oid`, an `i64`
//!    Unix timestamp, and a real `Vec<GitRef>` with ref names/kinds. A later
//!    repair pass gave `capture.rs` a `CaptureResult::commit_metas:
//!    Vec<CommitMeta>` (capture.rs:434-453) — pixel-aligned per-commit text
//!    read straight off the rendered sheet — but `CommitMeta` cannot be
//!    turned into a `GraphRow` honestly: its `short_sha` is a pre-truncated
//!    7-char display string with no path back to a real `Oid`, its
//!    `date_text` is a locale-formatted display string with no path back to
//!    an epoch, and it carries no parent list and no ref names at all (only
//!    a bare `has_refs: bool`). Fabricating those fields would either trip
//!    `detect_pivots`'s length assert (chapters.rs:201-207, it panics, not
//!    errors, on mismatch) if built wrong, or silently render invented tag
//!    names and dates on a callout card if built "successfully" — worse
//!    than the gap. Closing this for real needs either `chapters.rs`
//!    (lane 3's file, not this lane's) gaining a `CommitMeta`-shaped entry
//!    point, or this crate independently re-deriving real `GraphRow`s from
//!    the actual repository (a new dependency and a new `--repo` flag,
//!    materially bigger than a repair pass). So this CLI still always
//!    passes an empty pivot list to `detect_pivots`, which is exactly what
//!    `chapters::format_chapters` is documented to accept (chapters.rs:
//!    365-401): a `chapters.txt` sidecar is still written, just with only
//!    the mandatory `0:00 Start` line, and `pacing::build_timeline` runs
//!    with no dwells. The (still-empty) `pivots` value is threaded through
//!    to `EncodeConfig::pivots` too, so the callout-card renderer a
//!    concurrent lane built in `encode.rs` is fully wired end-to-end and
//!    will start firing the moment gap 1 above actually closes, with no
//!    second edit needed here. `--max-pivots` is parsed and stored for that
//!    day, but has no effect today.
//! 2. **`--date-overlay` is accepted but not drawn** (see `run`). Burning a
//!    per-frame marker means writing pixels *while cropping each frame* —
//!    `encode.rs`'s only public entry point, `encode_video`, has no overlay
//!    hook, and its frame-cropping loop (`FrameIter`, encode.rs:697-739) is
//!    private and outside this lane's file set (main.rs/README.md/
//!    Cargo.toml only). Pre-baking a date into the source PNG doesn't work
//!    either: the overlay needs to change every frame as the camera moves,
//!    and the source image is one flat sheet cropped by a moving window, not
//!    redrawn per frame. So this flag is parsed (scripts that pass it don't
//!    error) but produces a clear startup warning and is otherwise a no-op
//!    until `encode.rs` grows a hook for it.
//! 3. **`--out <dir>` works, via a documented CWD swap** (see `encode_in`).
//!    `encode_video` hardcodes its output location as a *relative* `./out/`
//!    (encode.rs:769-770) with no directory parameter — not a bug, just not
//!    this lane's file to add a parameter to. So this CLI resolves and
//!    validates the caller's `--out` up front, switches the process's
//!    current directory to it immediately before (and only before) the
//!    encode call so encode.rs's own relative `./out/` lands inside it, then
//!    moves the produced file up to `<out>/<name>` and removes the
//!    now-empty nested `out/`. Every other path used after that point is
//!    already absolute, so the directory swap cannot affect anything else in
//!    this run.
//!
//! # Two review findings fixed in this same repair pass (not gaps — bugs)
//!
//! Unlike the three items above, these are not "won't fix without another
//! lane's file" — they were confirmed defects this lane's own file set could
//! fix outright:
//!
//! - **`--duration`/captured width are validated as early as possible.**
//!   `validate_duration` rejects non-finite or non-positive `--duration`
//!   before Chromium is even resolved (NaN/negative would otherwise
//!   silently corrupt `pacing::build_timeline`'s arithmetic or quietly
//!   produce a zero-length timeline that only errors once `encode_video`
//!   reports zero frames). `run_pipeline` also checks the *measured*
//!   `capture_result.width` against `encode::VIDEO_WIDTH` immediately on
//!   return from `capture_print_sheet` — before pacing or encode runs —
//!   because the requested `--width` (checked at startup by
//!   `validate_width`) and the browser's actual rendered width can still
//!   diverge (horizontal overflow in the sheet's own content), and that
//!   divergence previously surfaced only deep inside `encode::
//!   extract_frame`, after a multi-minute capture had already run.
//! - **The scroll timeline is built against the *scrollable* height, not
//!   the full captured height.** `encode::clamp_crop_y` can only place the
//!   camera between `0` and `image_height - VIDEO_HEIGHT`; a timeline built
//!   against the full image height kept demanding positions past that for
//!   roughly the last `VIDEO_HEIGHT` px worth of scroll-time, all clamping
//!   to the same crop — a frozen tail. `run_pipeline` now computes
//!   `scrollable_height_px = capture_result.height.saturating_sub(VIDEO_
//!   HEIGHT)` once and passes it to both `build_multipliers` (and therefore
//!   `pacing::commit_density`) and `pacing::build_timeline`, matching the
//!   mechanism the encode lane's own repair-pass test proves
//!   (`last_frame_crop_y_reaches_exactly_the_scrollable_bottom_when_the_
//!   caller_passes_the_reduced_height`, encode.rs).

// `#[allow(unused_variables)]`: capture.rs is lane 1's file, outside this
// lane's set (main.rs/README.md/Cargo.toml only), and its own `run_capture`
// takes an `opts: &CaptureOptions` parameter it never reads internally
// (viewport width and chrome path are both already consumed one level up,
// in `capture_print_sheet`, before `run_capture` is called) — a genuine,
// harmless leftover in that file, not something wiring it in from here can
// or should paper over by editing capture.rs itself. Allowed here, at the
// `mod` boundary, rather than silently suppressed crate-wide, so it stays
// scoped to exactly the one warning it exists for.
#[allow(unused_variables)]
mod capture;
mod chapters;
mod encode;
mod pacing;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;

use capture::{CaptureOptions, DEFAULT_VIEWPORT_WIDTH};
use encode::{AudioSource, EncodeConfig, VIDEO_HEIGHT, VIDEO_WIDTH};

/// Width of one commit-density bucket, in image pixels (`pacing::
/// commit_density`'s `band_height` parameter). Not imported from
/// `crates/git-vista/src/geometry.rs::ROW_HEIGHT` (56px, cited there) — that
/// constant belongs to the wasm/Leptos app crate, which this native CLI must
/// not depend on (rule 5: no touching `crates/git-vista/`, and pulling in a
/// UI-framework crate for one integer would be a far worse coupling than
/// re-deriving it). 300px is roughly five-to-six rows at that spacing: fine
/// enough that a single busy afternoon shows up as its own band, coarse
/// enough that one stray commit in an otherwise quiet band doesn't yank the
/// speed curve around. Both `--linear` and density pacing use the same
/// bucket width, so a run only ever differs in the *multipliers* fed to
/// `pacing::build_timeline`, never in how the image is partitioned — see
/// `build_multipliers`'s doc comment.
const DENSITY_BAND_HEIGHT_PX: f64 = 300.0;

/// Filename the finished video is written under, inside `--out`. Fixed
/// rather than a flag: the owner's spec gives `--out <dir>`, not `--out-
/// name`, and a bare, crate-chosen name is what keeps `EncodeConfig::
/// out_name`'s own "no path separators" guard (encode.rs:758-764)
/// meaningful — this CLI never has to re-justify accepting a caller-supplied
/// filename.
const OUTPUT_VIDEO_NAME: &str = "scrollcast.mp4";

/// Renders git-vista's Print Graph sheet as a paced vertical-scroll
/// time-lapse video, suitable for narrating over.
///
/// Reads a print sheet already rendered to disk (see `capture.rs`'s doc
/// comment for why this tool cannot render it itself — no server route
/// exists to fetch it from, and starting one would bind the owner's live
/// port 8080), paces a scroll down it, and encodes an H.264/yuv420p MP4.
#[derive(Parser, Debug)]
#[command(name = "gv-scrollcast", version, about, long_about = None)]
struct Cli {
    /// The rendered Print Graph sheet to scroll — an HTML file wrapping
    /// print.rs's `graph_sheet()` output (preferred), or a bare SVG
    /// (`capture.rs`'s doc comment covers the tradeoff of the SVG shape).
    input: PathBuf,

    /// Target video length in seconds. Dwell time (pivot callouts) is
    /// carved out of this budget, not added on top — see `pacing::
    /// build_timeline`'s doc comment (pacing.rs:119-126).
    #[arg(long, default_value_t = 240.0)]
    duration: f64,

    /// Constant-rate scroll: disable commit-density pacing and move through
    /// every pixel band at the same speed. See `build_multipliers` for how
    /// this is expressed through the *same* timeline machinery rather than
    /// a second code path.
    #[arg(long, default_value_t = false)]
    linear: bool,

    /// Burn a subtle corner date marker into every frame. Accepted for CLI-
    /// contract stability; not yet honoured — see this file's top doc
    /// comment, gap 2, for exactly why and what would need to change.
    #[arg(long, default_value_t = false)]
    date_overlay: bool,

    /// Mux this audio file instead of the default silent placeholder track.
    /// Muxed at its own natural length — never stretched, padded, or
    /// truncated to match the video; a length mismatch is reported, not
    /// corrected (encode.rs's `audio_delta_message`).
    #[arg(long)]
    audio: Option<PathBuf>,

    /// Rendered viewport width, in CSS pixels. Must currently equal
    /// `encode::VIDEO_WIDTH` (1920) — see this file's `validate_width` for
    /// why a different value fails at startup instead of after a capture.
    #[arg(long, default_value_t = DEFAULT_VIEWPORT_WIDTH)]
    width: u32,

    /// Cap on pivot callouts (merges/tags/month boundaries). Parsed and
    /// stored, but currently has no effect — pivot detection needs data
    /// this pipeline doesn't yet produce; see this file's top doc comment,
    /// gap 1. 12 (chapters.rs:194-200's own worked example) is kept as the
    /// default so a future wiring of `detect_pivots` needs no flag-default
    /// change to match the number that module's own doc comment reasons
    /// about.
    #[arg(long, default_value_t = 12)]
    max_pivots: usize,

    /// Output directory for the finished video, `chapters.txt`, and the
    /// intermediate capture PNG. Must be creatable and must not fall inside
    /// this repository's own working tree (checked at startup — see
    /// `resolve_out_dir`).
    #[arg(long, default_value = "./out")]
    out: PathBuf,
}

fn main() {
    if let Err(err) = run() {
        // `{err:?}` (not `{err}`) prints anyhow's full context chain — every
        // `.context(...)` call along the failing path — which is what makes
        // "fail fast and clearly" actually mean something to whoever reads
        // the terminal instead of just the innermost error string.
        eprintln!("gv-scrollcast: error: {err:?}");
        std::process::exit(1);
    }
}

/// Everything that can fail before any Chromium/ffmpeg work starts: parsing,
/// binary resolution, and the `--out` checks. Kept synchronous and run
/// *before* a tokio runtime is ever built — matching `capture::
/// resolve_chrome_binary`'s and `encode::resolve_ffmpeg`'s own doc comments,
/// both of which are deliberately plain sync functions for exactly this
/// reason (capture.rs:296-303, encode.rs:184-198). A capture run is minutes
/// of CPU on this box; every one of these checks exists to fail before that
/// clock starts, not after.
fn run() -> Result<()> {
    let cli = Cli::parse();

    validate_width(cli.width)?;
    validate_duration(cli.duration)?;
    if cli.date_overlay {
        eprintln!(
            "gv-scrollcast: warning: --date-overlay was requested but is not yet implemented \
             (see main.rs's top doc comment, gap 2) — continuing without it."
        );
    }

    let chrome_path = capture::resolve_chrome_binary(None)
        .context("resolving the headless Chromium binary (checked before any capture work)")?;
    let ffmpeg_path = encode::resolve_ffmpeg()
        .context("resolving the ffmpeg binary (checked before any capture or encode work)")?;
    eprintln!("gv-scrollcast: chrome  -> {}", chrome_path.display());
    eprintln!("gv-scrollcast: ffmpeg  -> {}", ffmpeg_path.display());

    let out_dir = resolve_out_dir(&cli.out)?;
    eprintln!("gv-scrollcast: out dir -> {}", out_dir.display());

    // Built manually (not `#[tokio::main]`) so every check above runs on a
    // plain thread, with no runtime spun up at all if one of them fails.
    let runtime = tokio::runtime::Runtime::new().context("starting the tokio runtime")?;
    runtime.block_on(run_pipeline(cli, chrome_path, ffmpeg_path, out_dir))
}

/// `encode.rs` fixes the camera's output size as constants, not a parameter
/// (encode.rs:106-113: "a caller-supplied width/height here would silently
/// desynchronise the two without any type ever noticing"), and `extract_
/// frame` hard-errors on any captured width that isn't exactly `VIDEO_WIDTH`
/// (encode.rs:441-447, "instead of silently scaling"). A `--width 1280` run
/// would therefore capture successfully, decode successfully, and only fail
/// once the first frame is cropped — after the exact multi-minute capture
/// pass this crate exists to avoid re-running. Checking here turns that into
/// an immediate, named startup error instead.
fn validate_width(width: u32) -> Result<()> {
    if width != VIDEO_WIDTH {
        bail!(
            "--width {width} was requested, but encode.rs's camera is a fixed {VIDEO_WIDTH}x\
             {VIDEO_HEIGHT} (encode.rs:112-113) and hard-rejects any other captured width instead \
             of scaling to fit (encode.rs:441-447, by design — see that module's doc comment). \
             Pass --width {VIDEO_WIDTH} or omit the flag."
        );
    }
    Ok(())
}

/// Validate `--duration` before any Chromium/ffmpeg work starts (review
/// finding, Job A): `clap`'s `f64` parser accepts `"nan"`, `"inf"`,
/// `"-inf"`, and `"infinity"` (case-insensitively) as well as zero and
/// negative numbers — none of which `pacing::build_timeline` can turn into a
/// sane timeline. A `NaN` duration would make every `scroll_budget`/
/// `band_secs` computation in `build_timeline` (pacing.rs:127-197) itself
/// `NaN`, which then compares `false` against every `<=`/`>` it meets,
/// silently producing whatever fallback branch happens to run first — not a
/// clean error. Zero or negative durations aren't rejected by `build_timeline`
/// either: `scroll_budget = (target_duration_secs - total_dwell).max(0.0)`
/// (pacing.rs:139) just clamps to zero and the function returns a technically
/// valid but useless zero-length `Vec<Segment>`, which `encode_video` then
/// turns into its own `frame_count == 0` error (encode.rs:1228) — but only
/// *after* capture has already spent its minutes of CPU. Rejecting all of
/// these here, before Chromium is even resolved, is strictly earlier than
/// either of those two silent-or-late failure modes.
fn validate_duration(duration: f64) -> Result<()> {
    if !duration.is_finite() || duration <= 0.0 {
        bail!(
            "--duration {duration} is not usable: it must be a finite, positive number of \
             seconds. NaN/infinite values (clap's f64 parser accepts \"nan\"/\"inf\" literally) \
             would silently corrupt every downstream timeline computation in pacing::\
             build_timeline, and zero/negative values produce a zero-length timeline that only \
             fails once encode_video reports 0 frames — both are being rejected here, before any \
             Chromium/ffmpeg work starts, instead of after it."
        );
    }
    Ok(())
}

/// Resolve and validate `--out`: must be creatable, and must not fall inside
/// this repository's own working tree. The containment check runs *before*
/// creating anything, so a forbidden path is never even `mkdir -p`'d.
///
/// "This repository's tree" is resolved from `CARGO_MANIFEST_DIR`, a
/// compile-time constant baked into the binary at build time — the tree
/// this crate was built from — rather than searched for from the process's
/// runtime working directory. A CWD-based search would depend on where the
/// operator happened to invoke the binary from and could miss the tree
/// entirely if run from elsewhere on disk; the compiled-in path cannot.
fn resolve_out_dir(requested: &Path) -> Result<PathBuf> {
    resolve_out_dir_against(requested, find_repo_root().as_deref())
}

/// The pure decision `resolve_out_dir` makes, with the repo root taken as a
/// parameter rather than rediscovered via `find_repo_root()` — split out
/// purely for host-testability (this crate's standing style: pull the
/// decision out from behind whatever makes it non-reproducible, same shape
/// as capture.rs's `verify_capture_height` extraction). `resolve_out_dir`
/// itself always calls this with `find_repo_root()`'s real answer; tests
/// call it with a throwaway fixture repo's root instead, so "in-repo,
/// non-ignored → refused" and "in-repo, gitignored → accepted" can be
/// exercised without depending on *this* crate's own real `.git` (which
/// `find_repo_root()` would otherwise always resolve to, making a fixture
/// repo unreachable from inside a test run from this workspace).
///
/// `create_dir_all` still runs for real even under test — this function's
/// contract is "creatable", not merely "nameable" — so callers (tests
/// included) that want to observe rejection without touching disk must pass
/// a `requested` path expected to fail before that point, and callers that
/// want to observe acceptance must clean up the directory this creates
/// themselves (see the tests below).
fn resolve_out_dir_against(requested: &Path, repo_root: Option<&Path>) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("reading the current working directory")?;
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        cwd.join(requested)
    };
    // Not `canonicalize`d: the directory may not exist yet, and canonicalize
    // requires that it does. Component-wise joining is sufficient for the
    // containment check below; it does not need to resolve symlinks to be a
    // meaningful "don't write into the repo" guard.
    let candidate = normalize_lexically(&candidate);

    if let Some(repo_root) = repo_root {
        let repo_root = normalize_lexically(repo_root);
        // Inside the repo is allowed ONLY where git itself already ignores the
        // path. The rule being enforced is "never write into the *tracked*
        // tree" — not "never write under the repo root", which would reject
        // this tool's own documented default (`./out`) and leave the happy
        // path unrunnable from the one directory people actually run it from.
        //
        // Asking git, rather than comparing prefixes, is deliberate and fixes
        // two things at once. It makes the answer authoritative (the
        // `.gitignore` entry for `/out/` is what grants permission, so the
        // rule and the permission live in one place), and it closes the
        // symlink hole a lexical check cannot see: `--out /tmp/link` where
        // `/tmp/link` points into the repo passes any `starts_with` test but
        // is correctly refused here, because git resolves the real path.
        if candidate.starts_with(&repo_root) && !git_ignores(&repo_root, &candidate) {
            bail!(
                "--out {} resolves to {} which is inside this repository's working tree ({}) \
                 and is NOT gitignored — rendered video/PNG output must never land in the \
                 tracked tree. Either pass a directory outside the repo, or add this path to \
                 .gitignore (the default `./out` is already there).",
                requested.display(),
                candidate.display(),
                repo_root.display(),
            );
        }
    }

    std::fs::create_dir_all(&candidate)
        .with_context(|| format!("--out directory is not creatable: {}", candidate.display()))?;
    Ok(candidate)
}

/// Whether git itself ignores `path` inside `repo_root`.
///
/// `git check-ignore` rather than a hand-rolled `.gitignore` parser: the
/// precedence rules (negation, directory-only patterns, nested ignore files,
/// `core.excludesFile`) are git's, and a second implementation of them would
/// eventually disagree with the real one — in a guard whose whole job is
/// deciding whether a write is safe. Asking the authority is cheaper and
/// cannot drift.
///
/// **Fails closed.** If git cannot be run at all, this returns `false`, which
/// makes the caller refuse an in-repo path rather than allow one. A missing
/// git is a reason to be more careful about writing into a working tree, not
/// less.
///
/// **A trailing path separator is appended to the query, always.** `git
/// check-ignore` only matches a *directory-only* `.gitignore` pattern
/// (trailing `/`, exactly the shape this crate's own `.gitignore` entry uses
/// for its documented default — `/out/`) when it can tell the queried path
/// is a directory, and it can only tell that one of two ways: the path
/// already exists on disk and git stats it, or the query string itself ends
/// in a separator. `resolve_out_dir_against` calls this function
/// *deliberately before* `create_dir_all` (see that function's doc comment:
/// "a forbidden path is never even `mkdir -p`'d"), so on a fresh checkout's
/// very first run the directory does not exist yet — and without this fix,
/// `git check-ignore /repo/out` (no trailing slash, path absent) reports
/// "not ignored" even though `/out/` is right there in `.gitignore`,
/// wrongly rejecting this crate's own documented default `--out ./out`
/// before it had ever run once. Confirmed against this exact repository's
/// own `.gitignore` `/out/` entry while writing this repair pass's tests
/// (`resolve_out_dir_against_accepts_a_gitignored_in_repo_path` below is
/// what catches a regression). Always appending is safe because every
/// caller of this function only ever queries a directory (`--out` is a
/// directory, never a file) — there is no path this function is ever asked
/// about that a trailing separator could mischaracterize.
fn git_ignores(repo_root: &Path, path: &Path) -> bool {
    let mut query = path.as_os_str().to_os_string();
    if !query.to_string_lossy().ends_with(std::path::MAIN_SEPARATOR) {
        query.push(std::path::MAIN_SEPARATOR.to_string());
    }

    std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("check-ignore")
        .arg("-q")
        .arg(&query)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Walk upward from the compiled-in crate location looking for a `.git`
/// entry — the repository root this binary was built inside of. `None` if
/// none is found (e.g. this binary was copied out of a checkout entirely),
/// in which case `resolve_out_dir` has nothing to compare against and skips
/// the containment check rather than guessing.
fn find_repo_root() -> Option<PathBuf> {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Join `..`/`.` components away without touching the filesystem (unlike
/// `canonicalize`, which requires the path to exist and would also resolve
/// symlinks — more than this purely-textual "is one path a prefix of the
/// other" containment check needs, and a check that would fail outright on
/// the not-yet-created `--out` directory).
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The whole run, once every startup check has passed: capture -> pace ->
/// (chapters sidecar, pivots empty for now) -> encode. See this file's top
/// doc comment for the two gaps (`--date-overlay`, pivot detection) and the
/// one deliberate workaround (`--out`'s directory swap) this function's
/// shape reflects.
async fn run_pipeline(
    cli: Cli,
    chrome_path: PathBuf,
    ffmpeg_path: PathBuf,
    out_dir: PathBuf,
) -> Result<()> {
    // Redundant with the check `encode_video` runs internally (encode.rs:
    // 766-767) — deliberately so. That internal check only runs after
    // capture has already spent its minutes of CPU; running it here too
    // means a broken ffmpeg is reported before any of that work starts,
    // which is the whole point of "fail at startup, never mid-encode."
    encode::check_ffmpeg_capabilities(&ffmpeg_path)
        .await
        .context(
            "verifying ffmpeg has the capabilities this crate needs (libx264, aac, png decode, \
             mp4 mux, lavfi anullsrc) — see encode.rs's top doc comment for why the Playwright- \
             bundled ffmpeg specifically lacks all five",
        )?;

    // Canonicalized now, before this function's only CWD change (right
    // before the encode call, in `encode_in`) — a relative `--audio` path
    // would otherwise silently resolve against the wrong directory after
    // that point.
    let audio_source = match &cli.audio {
        Some(path) => {
            let abs = std::fs::canonicalize(path)
                .with_context(|| format!("--audio file not found: {}", path.display()))?;
            AudioSource::File(abs)
        }
        None => AudioSource::Silent,
    };

    eprintln!("gv-scrollcast: capturing {}", cli.input.display());
    let capture_output_path = out_dir.join("capture.png");
    let capture_opts = CaptureOptions {
        viewport_width: cli.width,
        chrome_path: Some(chrome_path),
    };
    let capture_result =
        capture::capture_print_sheet(&cli.input, &capture_output_path, &capture_opts)
            .await
            .context("capturing the Print Graph sheet")?;
    // From here on, `capture_result.png_path` (not `capture_output_path`) is
    // the path used — it is `capture_print_sheet`'s own confirmation of
    // where the PNG actually landed (capture.rs:565: `output_png.to_path_
    // buf()`), which is the more honest thing to build on than the request
    // that produced it.
    eprintln!(
        "gv-scrollcast: captured {}x{} PNG, {} commit node(s) found",
        capture_result.width,
        capture_result.height,
        capture_result.commit_ys.len(),
    );

    // Review finding, Job A: `validate_width` (above, before Chromium is even
    // resolved) checks the *requested* `--width`, but the browser's own
    // full-page screenshot can still come back a different width than the
    // viewport it was asked to render at — content wider than the requested
    // viewport causes horizontal overflow in Chromium's full-page capture,
    // and capture.rs never independently forces the output width the way it
    // forces (and verifies) height (capture.rs:809-818's `verify_capture_
    // height`, which this mirrors for width, has no width counterpart today).
    // Left unchecked, that width mismatch would only surface once `encode::
    // extract_frame` hard-rejects it (encode.rs:441-447) — *after* the
    // capture that just spent minutes of Chromium CPU. Checking the measured
    // width here, immediately on return from `capture_print_sheet` and before
    // any of the pacing/encode work below, turns that into an immediate,
    // named error naming both numbers instead.
    if capture_result.width != VIDEO_WIDTH {
        bail!(
            "captured PNG is {}px wide, but encode.rs's camera is fixed at {VIDEO_WIDTH}px \
             (encode.rs:112-113) and hard-rejects any other width instead of scaling to fit \
             (encode.rs:441-447) — the requested --width {} was validated at startup \
             (`validate_width`), but the page's own rendered content came back a different \
             width than the viewport it was asked to render at (most likely horizontal overflow \
             in the sheet itself). Refusing now, before any pacing/encode work runs, rather than \
             failing deep inside frame extraction after the capture's minutes of CPU are already \
             spent.",
            capture_result.width,
            cli.width,
        );
    }

    // Review finding, Job B (encode lane's report): `pacing::build_timeline`'s
    // own y-range must stop where the camera can actually still crop to —
    // `encode::clamp_crop_y` (encode.rs:450) pins the crop window at
    // `image_height - VIDEO_HEIGHT`, so a timeline built against the FULL
    // captured height keeps demanding camera positions past that point for
    // the last `VIDEO_HEIGHT` px worth of scroll-time, all of which clamp to
    // the exact same crop — a frozen tail while the clock keeps running. See
    // `encode.rs`'s `last_frame_crop_y_reaches_exactly_the_scrollable_bottom_
    // when_the_caller_passes_the_reduced_height` test (encode.rs, added by
    // the encode lane's repair pass) for the mechanism this fixes.
    // `saturating_sub` floors at 0 for a capture shorter than one viewport
    // (nothing to scroll to at all), matching the encode lane's own
    // `.max(0.0)` — done here in `u32` since both operands already are.
    let scrollable_height_px = capture_result.height.saturating_sub(VIDEO_HEIGHT);

    let multipliers =
        build_multipliers(&capture_result.commit_ys, scrollable_height_px, cli.linear);

    // Gap 1 (this file's top doc comment) — STILL a real gap, only
    // partially narrowed by a concurrent lane's work, not closed by it.
    // `capture::CaptureResult` now carries `commit_metas: Vec<CommitMeta>`
    // (capture.rs:434-453), index-aligned with `commit_ys`. But `chapters::
    // detect_pivots` (chapters.rs:201) still takes `&[GraphRow]`, and
    // `CommitMeta` cannot honestly be turned into one — not a missing
    // conversion function, a missing *fact*:
    //   - `GraphRow.commit.id` is a real `Oid` (git-vista-core/src/
    //     model.rs:12); `CommitMeta::short_sha` is print.rs's already-
    //     truncated 7-char display string (capture.rs:462-463) — there is no
    //     way to recover the other 33+ hex characters from it.
    //   - `GraphRow.commit.time` is an `i64` Unix timestamp that `detect_
    //     pivots`'s own month-boundary detection depends on (chapters.rs:
    //     221-222, `civil_from_unix`); `CommitMeta::date_text` is a browser-
    //     locale-formatted display string (capture.rs:470-474, e.g. "Jun 29
    //     14:32") with no reliable inverse back to an epoch value.
    //   - `GraphRow.refs` is `Vec<GitRef>` with a real name and `RefKind`
    //     (model.rs:90-98), which `render_label`/`render_detail` read
    //     directly to print e.g. `"Tag: v1.2.0"` (chapters.rs:271-274);
    //     `CommitMeta::has_refs` is a bare `bool` (capture.rs:475-479) — no
    //     ref name or kind survives the page probe at all.
    //   - `GraphRow.commit.parents` (a real `Vec<Oid>`) doesn't exist in
    //     `CommitMeta` either; `is_merge: Option<bool>` (capture.rs:480-486)
    //     is derived honestly from the node's icon glyph instead, and is
    //     real — but it is the one field of five that transfers.
    // Fabricating the other four (a made-up `Oid` from `short_sha`, a
    // parsed-back epoch from `date_text`, an invented ref name/kind from
    // `has_refs`, an empty-or-guessed `parents`) is exactly what capture.rs's
    // own doc comment (capture.rs:109-117) warns is worse than the gap:
    // either it trips `detect_pivots`'s length assert (chapters.rs:201-207,
    // it panics, not errors, on mismatch) if built wrong, or it silently
    // renders fabricated tag names/dates on a callout card someone narrates
    // over, while *looking* wired. Bridging this for real needs one of two
    // things neither of which is this lane's file to make: `chapters::
    // detect_pivots` gaining a `CommitMeta`-shaped entry point (chapters.rs,
    // lane 3's file), or a genuinely independent source of real `GraphRow`s
    // (e.g. re-walking the actual repo via `git-vista-core`/`git-vista-git`,
    // which this crate does not currently depend on and which would need a
    // new `--repo` flag to even know which repository's history matches the
    // sheet being scrolled — a materially bigger feature than a repair pass
    // over this crate's four confirmed findings). So: still called with two
    // genuinely empty slices, still an honest statement of "zero rows'
    // worth of real metadata are available to this call," not a fabricated
    // one. `cli.max_pivots` remains threaded through, still with no effect
    // on an empty input.
    let pivots = chapters::detect_pivots(&[], &[], cli.max_pivots);

    let segments = pacing::build_timeline(
        scrollable_height_px as f64,
        DENSITY_BAND_HEIGHT_PX,
        &multipliers,
        &pivots,
        cli.duration,
        pacing::DEFAULT_DWELL_SECS,
    );
    eprintln!(
        "gv-scrollcast: timeline built: {} segment(s), {:.1}s total",
        segments.len(),
        pacing::total_duration(&segments),
    );

    // Written even though `pivots` is always empty right now: `format_
    // chapters` is documented to always emit the mandatory `0:00 Start`
    // line for an empty pivot list (chapters.rs:365-401), so the sidecar is
    // a real, valid (if minimal) file rather than something skipped
    // alongside the pivot detection gap it's downstream of.
    let chapters_txt = chapters::format_chapters(&pivots, &segments);
    let chapters_path = out_dir.join("chapters.txt");
    std::fs::write(&chapters_path, &chapters_txt)
        .with_context(|| format!("writing {}", chapters_path.display()))?;

    let encode_config = EncodeConfig {
        audio: audio_source,
        out_name: OUTPUT_VIDEO_NAME.to_string(),
        // Threaded through per the encode lane's repair-pass report (its
        // "IMPORTANT — activating this requires a one-line change in
        // main.rs" note): `EncodeConfig::pivots` needs the same `pivots`
        // this function already builds for `pacing::build_timeline` and
        // `chapters::format_chapters`, so the callout-card mechanism reads
        // from the one real source of pivots this pipeline has, rather than
        // silently defaulting to `Vec::new()` forever even after a future
        // lane closes the gap documented above. `pivots` is empty today for
        // the reason documented at this function's `chapters::detect_
        // pivots` call site — this line does not fabricate anything, it
        // just stops the empty list from being reconstructed twice (once
        // implicitly via `EncodeConfig::default()`, once really) and wires
        // the real variable through so the day gap 1 closes, this becomes
        // live with no second edit needed here.
        pivots: pivots.clone(),
        ..EncodeConfig::default()
    };

    eprintln!("gv-scrollcast: encoding (this is the slow part)...");
    let report = encode_in(
        &out_dir,
        &capture_result.png_path,
        &capture_result,
        &segments,
        &encode_config,
    )
    .await?;

    println!("gv-scrollcast: done");
    println!("  video:      {}", report.output_path.display());
    println!("  chapters:   {}", chapters_path.display());
    println!("  capture:    {}", capture_result.png_path.display());
    println!(
        "  {} frames, {:.1}s video",
        report.frame_count, report.video_duration_secs
    );
    if let Some(delta) = &report.audio_delta_message {
        println!("  audio:      {delta}");
    }

    Ok(())
}

/// Speed multiplier per pixel band, for either pacing mode. `--linear`
/// expresses "constant rate" through the *same* `pacing::build_timeline`
/// machinery rather than a second code path: a multiplier of exactly `1.0`
/// for every band gives every band an equal share of the scroll-time budget
/// per pixel (see `build_timeline`'s doc comment on `band_weight = 1.0 /
/// mult`, pacing.rs:141-144 — `mult == 1.0` for every band makes every
/// band's weight identical, which is what a flat, non-adaptive scroll rate
/// means in this module's time-budget model). The non-linear path is just
/// `pacing::commit_density` followed by `pacing::speed_multipliers`, exactly
/// as already built and tested in `pacing.rs`.
///
/// `scrollable_height_px` — **not** the full captured PNG height — for the
/// same reason `run_pipeline` passes it to `pacing::build_timeline` instead
/// of `capture_result.height`: `encode::clamp_crop_y` can only place the
/// camera between `0` and `image_height - VIDEO_HEIGHT`, so bucketing
/// density (and therefore band count) against the full height would count
/// bands the camera timeline built from `build_timeline` never actually
/// reaches at the same y-range this function's bands are keyed to. Passing
/// the same reduced height to both keeps this function's band boundaries
/// and `build_timeline`'s band boundaries in agreement — see this file's
/// `build_multipliers_produces_one_multiplier_per_pacing_band` test for the
/// exact cross-check with `pacing::commit_density`'s own band count.
fn build_multipliers(
    commit_ys: &[pacing::CommitY],
    scrollable_height_px: u32,
    linear: bool,
) -> Vec<f64> {
    if linear {
        let band_count =
            pacing::band_range(scrollable_height_px as f64, DENSITY_BAND_HEIGHT_PX).count();
        vec![1.0; band_count.max(1)]
    } else {
        let density = pacing::commit_density(
            commit_ys,
            scrollable_height_px as f64,
            DENSITY_BAND_HEIGHT_PX,
        );
        pacing::speed_multipliers(&density)
    }
}

/// Run `encode::encode_video` and move its output from encode.rs's hardcoded
/// relative `./out/` to `<out_dir>/<name>` — see this file's top doc
/// comment, gap 3, for exactly why this dance exists instead of a parameter
/// on `encode_video`.
///
/// The current directory is changed only for the duration of this function,
/// and only `encode_video` itself runs while it's changed — every path this
/// function touches directly (`out_dir`, `png_path`, the moved file) is
/// already absolute, so the swap cannot affect them. Nothing after this
/// function runs relies on the original directory, so it is not restored
/// beyond this function's own cleanup below.
async fn encode_in(
    out_dir: &Path,
    png_path: &Path,
    capture_result: &capture::CaptureResult,
    segments: &[pacing::Segment],
    config: &EncodeConfig,
) -> Result<encode::EncodeReport> {
    let original_cwd = std::env::current_dir().context("reading the current working directory")?;
    std::env::set_current_dir(out_dir)
        .with_context(|| format!("switching into --out directory {}", out_dir.display()))?;

    let result = encode::encode_video(
        png_path,
        capture_result.width,
        capture_result.height,
        segments,
        config,
    )
    .await;

    // Always attempt to restore the original directory before propagating
    // either outcome, mirroring capture.rs's own "always clean up on every
    // exit path" shape (capture.rs:455-467) for the same reason: a failed
    // encode should not also leave the process in a directory the caller
    // didn't ask for.
    let _ = std::env::set_current_dir(&original_cwd);

    let report = result?;

    // `report.output_path` is encode.rs's own relative "out/<name>" (encode.
    // rs:770) — joining it onto `out_dir` locates the file without depending
    // on the current directory at all, which is why this works whether or
    // not the restore above succeeded.
    let produced = out_dir.join(&report.output_path);
    let final_path = out_dir.join(&config.out_name);
    std::fs::rename(&produced, &final_path).with_context(|| {
        format!(
            "moving encoded video from {} to {}",
            produced.display(),
            final_path.display()
        )
    })?;
    if let Some(nested_out) = produced.parent() {
        // Best-effort: `remove_dir` only succeeds on an empty directory,
        // which this always is (encode_video never writes anything else
        // under its "out/"), but a failure here is cosmetic — the video
        // itself already landed at the right place — so it's reported, not
        // propagated.
        if let Err(e) = std::fs::remove_dir(nested_out) {
            eprintln!(
                "gv-scrollcast: warning: could not remove now-empty {}: {e}",
                nested_out.display()
            );
        }
    }

    Ok(encode::EncodeReport {
        output_path: final_path,
        ..report
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- validate_width / validate_duration --------------------------------
    //
    // Both are new in this repair pass (review Job A: fail before any
    // Chromium/ffmpeg work starts). Neither existed as a tested unit before
    // — these are the first tests either function gets.

    #[test]
    fn validate_width_accepts_exactly_the_fixed_video_width() {
        assert!(validate_width(VIDEO_WIDTH).is_ok());
    }

    #[test]
    fn validate_width_rejects_anything_else_and_names_both_numbers() {
        // Mutation this catches: comparing against the wrong constant (e.g.
        // VIDEO_HEIGHT) or dropping one of the two numbers from the message.
        let err = validate_width(1280).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("1280"), "{msg}");
        assert!(msg.contains(&VIDEO_WIDTH.to_string()), "{msg}");
    }

    #[test]
    fn validate_duration_accepts_a_normal_positive_value() {
        assert!(validate_duration(240.0).is_ok());
    }

    #[test]
    fn validate_duration_rejects_zero_and_negative() {
        // Mutation this catches: using `< 0.0` instead of `<= 0.0`, which
        // would let a zero duration through to `pacing::build_timeline`
        // (which clamps it to an empty, useless timeline rather than
        // erroring — see this fn's own doc comment).
        assert!(validate_duration(0.0).is_err());
        assert!(validate_duration(-5.0).is_err());
    }

    #[test]
    fn validate_duration_rejects_nan_and_infinite() {
        // Mutation this catches: dropping the `is_finite()` half of the
        // check and relying on `<= 0.0` alone, which is false for both NaN
        // (every comparison against NaN is false) and +infinity.
        assert!(validate_duration(f64::NAN).is_err());
        assert!(validate_duration(f64::INFINITY).is_err());
        assert!(validate_duration(f64::NEG_INFINITY).is_err());
    }

    // ---- normalize_lexically -----------------------------------------------

    #[test]
    fn normalize_lexically_collapses_dot_and_dotdot_without_touching_disk() {
        // Mutation this catches: dropping the `ParentDir => { out.pop(); }`
        // arm (or swapping it for a no-op), which would leave a literal
        // `..` component in the output instead of resolving it against the
        // component that came before it.
        let result = normalize_lexically(Path::new("/a/b/../c/./d"));
        assert_eq!(result, Path::new("/a/c/d"));
    }

    #[test]
    fn normalize_lexically_does_not_escape_past_root_via_excess_dotdot() {
        // `PathBuf::pop()` on a path that is only `/` is a documented no-op,
        // not a panic and not something that produces a `..` in the output
        // — this exercises exactly that boundary, which a naive
        // "always pop" implementation could get wrong if it didn't rely on
        // `pop()`'s own root-aware behavior.
        let result = normalize_lexically(Path::new("/../../a"));
        assert_eq!(result, Path::new("/a"));
    }

    #[test]
    fn normalize_lexically_leaves_an_already_clean_path_untouched() {
        let result = normalize_lexically(Path::new("/home/tom/videos"));
        assert_eq!(result, Path::new("/home/tom/videos"));
    }

    // ---- build_multipliers --------------------------------------------------

    #[test]
    fn build_multipliers_produces_one_multiplier_per_pacing_band() {
        // Cross-checked directly against `pacing::commit_density`'s own
        // band-count formula rather than a hardcoded expected number:
        // finding B's fix (main.rs now passes `scrollable_height_px`, not
        // the full capture height, to both `build_multipliers` and
        // `pacing::build_timeline`) depends on both call sites bucketing
        // pixels into the *same* bands. A silent drift between this
        // function's band count and `pacing::commit_density`'s own would
        // reopen a version of the exact desynchronization finding B fixed,
        // just one layer higher — this test is what would catch that.
        let commit_ys = vec![
            pacing::CommitY { y: 10.0 },
            pacing::CommitY { y: 250.0 },
            pacing::CommitY { y: 610.0 },
        ];
        let scrollable_height_px = 1_000u32;

        let multipliers = build_multipliers(&commit_ys, scrollable_height_px, false);
        let density = pacing::commit_density(
            &commit_ys,
            scrollable_height_px as f64,
            DENSITY_BAND_HEIGHT_PX,
        );
        assert_eq!(multipliers.len(), density.len());
    }

    #[test]
    fn build_multipliers_linear_mode_also_agrees_on_band_count() {
        // `--linear` takes a different code path (`pacing::band_range`
        // rather than `pacing::commit_density`) to reach what should be the
        // same band count — this proves the two paths actually agree with
        // each other, not just that each independently "produces something".
        let commit_ys = vec![pacing::CommitY { y: 500.0 }];
        let scrollable_height_px = 1_000u32;

        let linear = build_multipliers(&commit_ys, scrollable_height_px, true);
        let density = pacing::commit_density(
            &commit_ys,
            scrollable_height_px as f64,
            DENSITY_BAND_HEIGHT_PX,
        );
        assert_eq!(linear.len(), density.len());
    }

    #[test]
    fn build_multipliers_never_returns_empty_even_for_a_zero_scrollable_height() {
        // A capture no taller than one viewport has `scrollable_height_px`
        // saturated to 0 by `run_pipeline` (finding B's fix) — both
        // `pacing::band_range` and `pacing::commit_density` floor their own
        // band count at 1 (`.max(1.0)`), and this function's `.max(1)` on
        // the linear path exists to match that floor, not accidentally
        // diverge from it into a genuinely empty `Vec`.
        let multipliers = build_multipliers(&[], 0, true);
        assert_eq!(multipliers.len(), 1);
        let multipliers = build_multipliers(&[], 0, false);
        assert_eq!(multipliers.len(), 1);
    }

    // ---- resolve_out_dir_against --------------------------------------------

    /// Builds a throwaway git repository under `std::env::temp_dir()` with a
    /// `.gitignore` that ignores exactly one named subdirectory. A FIXTURE
    /// repo, never this crate's own — `find_repo_root()` walks from
    /// `CARGO_MANIFEST_DIR` and would always resolve to *this* repository's
    /// real root, which is exactly why `resolve_out_dir_against` takes
    /// `repo_root` as a parameter instead of calling `find_repo_root()`
    /// itself: it lets a test substitute a disposable repo instead of having
    /// to exercise "in-repo, refused" against this actual working tree.
    /// Built with plain `std::process::Command` git calls (`git init`),
    /// exactly as the task specifies — no new dependency.
    fn fixture_git_repo(unique_suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gv-scrollcast-out-dir-test-{unique_suffix}"));
        let _ = std::fs::remove_dir_all(&dir); // best-effort cleanup of a prior run's leftovers
        std::fs::create_dir_all(&dir).expect("create fixture repo dir");

        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .arg("init")
            .arg("-q")
            .status()
            .expect("run `git init` for the fixture repo");
        assert!(status.success(), "git init failed for fixture repo");

        // `git check-ignore` reads `.gitignore` straight off the working
        // tree — it does not need this file staged or committed to take
        // effect, so writing it is the whole setup.
        std::fs::write(dir.join(".gitignore"), "/ignored-out/\n")
            .expect("write fixture .gitignore");

        dir
    }

    #[test]
    fn git_ignores_matches_a_directory_only_pattern_even_though_the_directory_does_not_exist_yet() {
        // The bug this repair pass found and fixed: `git check-ignore` only
        // honours a directory-only pattern (trailing `/`, exactly this
        // crate's own `.gitignore` shape for `/out/`) when it can tell the
        // queried path is a directory — either by stat'ing an existing one,
        // or by the query itself ending in a separator. `resolve_out_dir_
        // against` calls `git_ignores` BEFORE creating anything, so without
        // appending that separator here, this would return `false` on every
        // fresh checkout's first run and reject this crate's own documented
        // `--out ./out` default. Mutation this catches: dropping the
        // trailing-separator append and passing `path` straight through.
        let repo = fixture_git_repo("git-ignores-nonexistent-dir");
        let candidate = repo.join("ignored-out"); // deliberately never created
        assert!(
            !candidate.exists(),
            "sanity: this test is about the absent-path case"
        );
        assert!(git_ignores(&repo, &candidate));

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn resolve_out_dir_against_refuses_an_in_repo_path_git_does_not_ignore() {
        let repo = fixture_git_repo("refused");
        let requested = repo.join("not-ignored-out");

        let err = resolve_out_dir_against(&requested, Some(&repo)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("NOT gitignored"), "{msg}");
        // Refused before ever creating anything — the whole point of
        // checking containment before `create_dir_all` runs.
        assert!(!requested.exists());

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn resolve_out_dir_against_accepts_a_gitignored_in_repo_path() {
        let repo = fixture_git_repo("accepted");
        let requested = repo.join("ignored-out");

        let resolved = resolve_out_dir_against(&requested, Some(&repo)).unwrap();
        assert_eq!(resolved, requested);
        assert!(
            resolved.is_dir(),
            "resolve_out_dir_against must actually create the accepted directory"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn resolve_out_dir_against_accepts_any_path_when_no_repo_root_is_known() {
        // `find_repo_root()` returns `None` when this binary was copied out
        // of a checkout entirely (its own doc comment) — the containment
        // check must be skipped entirely in that case, not treated as
        // "everything is in-repo, refuse it all".
        let dir = std::env::temp_dir().join("gv-scrollcast-out-dir-test-no-repo-root");
        let _ = std::fs::remove_dir_all(&dir);

        let resolved = resolve_out_dir_against(&dir, None).unwrap();
        assert_eq!(resolved, dir);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
