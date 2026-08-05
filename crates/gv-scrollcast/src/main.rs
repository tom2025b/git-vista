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
//! 1. **Pivot callouts never fire yet** (see `run_pipeline`). `chapters::
//!    detect_pivots` needs `&[git_vista_core::model::GraphRow]` — full
//!    commit metadata (refs, parents, message, time) — but `capture.rs`'s
//!    in-page probe (capture.rs:188-210) extracts only pixel y-positions.
//!    No module in this pipeline currently produces a `Vec<GraphRow>`
//!    index-aligned with `commit_ys`, and `detect_pivots` *panics* (not
//!    errors) on a length mismatch (chapters.rs:201-207, tested at
//!    chapters.rs:538-543) — so calling it with fabricated placeholder rows
//!    would either crash on the assert or silently produce zero pivots
//!    while looking wired. Neither is honest. This CLI always passes an
//!    empty pivot list instead, which is exactly what `chapters::
//!    format_chapters` is documented to accept (chapters.rs:365-401): a
//!    `chapters.txt` sidecar is still written, just with only the mandatory
//!    `0:00 Start` line, and `pacing::build_timeline` runs with no dwells.
//!    `--max-pivots` is parsed and stored for the day a lane adds that
//!    extraction to `capture.rs`, but has no effect today.
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

    if let Some(repo_root) = find_repo_root() {
        let repo_root = normalize_lexically(&repo_root);
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
fn git_ignores(repo_root: &Path, path: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("check-ignore")
        .arg("-q")
        .arg(path)
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

    let multipliers =
        build_multipliers(&capture_result.commit_ys, capture_result.height, cli.linear);

    // Gap 1 (this file's top doc comment): no module in this pipeline
    // produces `Vec<GraphRow>` index-aligned with `commit_ys`, so there is
    // no row metadata to hand `chapters::detect_pivots`. Called with two
    // genuinely empty slices — an honest statement of "zero rows' worth of
    // metadata are available" — rather than fabricated placeholder
    // `GraphRow`s padded out to `commit_ys`'s real (non-empty) length: that
    // would either trip `detect_pivots`'s equal-length assert (it panics,
    // not errors, on mismatch — chapters.rs:201-207) if built wrong, or
    // silently score zero everywhere while *looking* wired if built right,
    // which is worse. `cli.max_pivots` is threaded through for the day a
    // lane adds that extraction to `capture.rs`; with zero input rows it
    // has no effect yet.
    let pivots = chapters::detect_pivots(&[], &[], cli.max_pivots);

    let segments = pacing::build_timeline(
        capture_result.height as f64,
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
fn build_multipliers(commit_ys: &[pacing::CommitY], image_height: u32, linear: bool) -> Vec<f64> {
    if linear {
        let band_count = pacing::band_range(image_height as f64, DENSITY_BAND_HEIGHT_PX).count();
        vec![1.0; band_count.max(1)]
    } else {
        let density =
            pacing::commit_density(commit_ys, image_height as f64, DENSITY_BAND_HEIGHT_PX);
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
