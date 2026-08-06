//! Frame generation and video encoding: turn a built [`pacing::Segment`]
//! timeline plus one full-height PNG into an MP4 that plays on the owner's
//! iPad, in a browser, and everywhere else (#325, Lane 2).
//!
//! ## Why H.264 + yuv420p + MP4, non-negotiably
//!
//! The owner's requirement is a "video format that works anywhere." That is
//! not a taste preference — it rules out choices that look equivalent on a
//! desktop encoder's command line but are not. `yuv420p` in particular is
//! load-bearing, not cosmetic: several encoders (including some libx264
//! presets when fed an RGB source) default to `yuv444p` because it is
//! lossless with respect to chroma, which sounds strictly better. It is not
//! playable at all on iOS/Safari or older QuickTime, both of which hard-require
//! 4:2:0 chroma subsampling for H.264. A future edit that "optimises" the
//! pixel format to keep more chroma detail would silently break playback on
//! the owner's own iPad — the primary device this whole crate exists for —
//! and nothing in a passing `cargo test` run would catch it, because the
//! failure is a codec-negotiation refusal on a specific device class, not a
//! Rust-level error. That is why [`build_encode_args`] hardcodes
//! `-vf format=yuv420p` and why a test below asserts `yuv444p` never appears
//! in the built argument list.
//!
//! ## Why this crate decodes and crops with ffmpeg, not a Rust image crate
//!
//! This module needs pixel data out of a PNG. The obvious route is a Rust
//! PNG-decoding crate — but this crate's dependency list
//! (`crates/gv-scrollcast/Cargo.toml`) is owned by a different lane of this
//! same issue (#325) and this module's mandate is `encode.rs` only; adding a
//! dependency is exactly the kind of cross-lane edit the issue's file-
//! ownership split exists to prevent. `ffmpeg` itself is a fully capable PNG
//! decoder (in any non-stripped build — see the capability-probe section
//! below), and it is already the one binary this crate is guaranteed to
//! shell out to. So decoding is one `ffmpeg` subprocess call
//! (`-i in.png -f rawvideo -pix_fmt rgb24 pipe:1`) that hands back a flat
//! RGB24 byte buffer, and every pixel operation after that — computing which
//! rows belong to frame *n* and slicing them out — is plain Rust byte-slice
//! arithmetic with no image-format knowledge at all. That keeps the only
//! genuinely novel logic in this module (the per-frame crop) pure and
//! host-testable (see `extract_frame`'s tests), which matches this crate's
//! standing split between decided-in-Rust and carried-out-by-a-subprocess.
//!
//! ## Frame delivery: pipe raw frames to ffmpeg's stdin, never a temp-dir of PNGs
//!
//! The arithmetic the task calls out is real and decides this outright. At
//! 1920x1080 rgb24, one raw frame is `1920 * 1080 * 3 = 6,220,800` bytes —
//! the "~6MB" figure. A 4-minute video at 30fps is 7,200 frames. Materialising
//! all of them before encoding — whether as 7,200 loose files in a temp dir or
//! as one 43GB raw blob — is a non-starter on this box's stated shape (4
//! cores, 8GB RAM, spinning disk): 43GB does not fit in RAM at all, and 7,200
//! individual file creates on a spinning disk is seek-bound and slow even if
//! disk space were free. Piping avoids both failure modes at once: this
//! module decodes the *source* PNG exactly once into one in-memory buffer
//! (typically tens to low hundreds of MB — see [`MAX_DECODE_BYTES`] for the
//! guard against a pathological source), and every output frame after that is
//! a zero-copy slice of rows already sitting in that buffer (see
//! `extract_frame`), written straight into ffmpeg's stdin one frame at a time.
//! Nothing beyond the OS pipe buffer and whatever ffmpeg itself is holding
//! mid-encode is ever resident — the 7,200-frame video and the 4-minute one
//! cost the same peak memory.
//!
//! ## Determinism: what "same input -> same bytes" does and does not cover
//!
//! `-threads 1` (no multi-threaded encode nondeterminism), a fixed CRF
//! instead of a bitrate target (no rate-control feedback loop to introduce
//! run-to-run variance), `-fflags +bitexact` and `-flags:v +bitexact`
//! (suppress muxer/encoder metadata that some builds stamp with a library
//! version string), and a fixed `creation_time` (never the real wall clock)
//! together make one *pinned* ffmpeg binary produce byte-identical output for
//! byte-identical input. What this does **not** cover: a different ffmpeg
//! build or libx264 version can produce different bytes from the same flags
//! and the same source, because encoder heuristics and SIMD codepaths change
//! between versions. "Same input -> same bytes" is a claim about one binary,
//! not about H.264 as a format — see [`resolve_ffmpeg`]'s doc comment for why
//! the resolved binary's identity is exactly the thing this module cannot
//! promise stays fixed across environments.
//!
//! ## Fail at startup, never mid-encode
//!
//! [`check_ffmpeg_capabilities`] runs before any frame is decoded or written.
//! This exists because of a fact discovered while building this module: the
//! ffmpeg binary this repo's own tooling bundles for headless-browser video
//! capture (Playwright's, at the fallback path in [`resolve_ffmpeg`]) is
//! built with `--disable-everything` and a narrow allow-list scoped to
//! Playwright's *own* screen-recording need (webm/vp8 output from an mjpeg
//! screenshot stream). It has no `libx264` encoder, no `aac` encoder, no
//! `mp4`/`mov` muxer, and — the one that would otherwise fail furthest into a
//! run — no `png` decoder at all (it can *encode* PNG, for screenshot output,
//! but not read one back in). None of that is a defect in this module; it is
//! this dev box's available binary being scoped to a different job. Rather
//! than let that surface as an inscrutable "Unknown decoder 'png'" three
//! minutes into a decode, this module probes `-encoders`/`-decoders`/`-muxers`
//! up front and fails with a message naming exactly what is missing and
//! where it looked, so a caller in this environment gets a clear, immediate
//! answer, and a caller with `$GV_SCROLLCAST_FFMPEG` pointed at a full build
//! gets exactly the pipeline described above.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::pacing::{camera_y_at, total_duration, Pivot, Segment};

/// The camera's fixed output size. Not configurable: the whole pacing model
/// (`pacing::build_timeline`'s `band_height` argument) is built by the caller
/// against this exact viewport height, and a caller-supplied width/height
/// here would silently desynchronise the two without any type ever noticing.
/// See `extract_frame`'s width check for the runtime guard on the other half
/// of that assumption (the *source* PNG's width).
pub const VIDEO_WIDTH: u32 = 1920;
pub const VIDEO_HEIGHT: u32 = 1080;
pub const FPS: u32 = 30;

/// Upper bound on the decoded source PNG's raw RGB24 size. This is a sanity
/// fence, not a tuned limit: at ~186MB/GiB-row-of-1920px... concretely,
/// 1920px wide, this permits a source image roughly 218,000px tall before
/// tripping — far beyond any commit graph this tool is meant to render. Its
/// job is to turn "the box OOMs an hour into a run because a bug fed it a
/// bogus height" into an immediate, named error, on a machine that runs a
/// live server other work depends on.
pub const MAX_DECODE_BYTES: u64 = 1_500_000_000;

/// Where the caller's audio comes from. `Silent` is not "no audio" — the
/// task's contract is a **present** AAC track for the full video duration
/// so a voiceover can be dropped in later without a re-encode of the video
/// stream, so `Silent` still produces a real (silent) audio track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioSource {
    Silent,
    File(PathBuf),
}

/// Tunable encode knobs. Deliberately small: width/height/fps are fixed
/// constants above (see their doc comment for why), and everything here is
/// either a genuine quality/determinism trade (`crf`, `preset`) or unavoidably
/// per-run (`audio`, `out_name`, `pivots`).
///
/// `Eq` is deliberately NOT derived here (unlike before `pivots` was added):
/// `Pivot` (`pacing.rs`) carries an `f64` field and only derives `PartialEq`,
/// for the same reason `Segment` in the same module isn't `Eq` either — `f64`
/// has no total order. Adding a `Vec<Pivot>` field makes that the same
/// constraint that already applies one type over.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodeConfig {
    /// x264 constant-rate-factor. Fixed per run (never a bitrate target) —
    /// see this module's top doc comment on why CRF and not bitrate.
    pub crf: u32,
    /// x264 preset name (e.g. "medium"). Slower presets are smaller/cleaner
    /// for the same CRF; this is a speed/size trade, not a determinism one —
    /// any fixed preset is equally deterministic under `-threads 1`.
    pub preset: String,
    pub audio: AudioSource,
    /// Filename only (no path components) — joined under `./out/` by
    /// [`encode_video`]. Keeping this a bare name rather than accepting a
    /// caller-supplied path is what makes "output strictly under `./out/`"
    /// a property of the function's signature rather than a convention a
    /// caller could accidentally violate.
    pub out_name: String,
    /// Pivot callouts to composite onto their matching dwell frames (see
    /// `build_dwell_pivot_text` and the "Pivot callout cards" section
    /// below). A field on `EncodeConfig` rather than a new parameter on
    /// [`encode_video`] on purpose: `encode_video`'s only caller is
    /// `main.rs`, outside this lane's file set for #325's repair pass
    /// (`main.rs` is the crate root here — there is no `lib.rs` — so
    /// `cargo build`/`cargo test` compile it and `encode.rs` as one unit,
    /// and a changed *function signature* would leave the crate unable to
    /// compile until `main.rs`'s call site was updated to match). A new
    /// *struct field* on `EncodeConfig` does not have that problem: every
    /// existing `EncodeConfig { .., ..EncodeConfig::default() }` literal
    /// (main.rs:468-472 is the one real caller) picks it up automatically
    /// at its default (empty), which is exactly today's real behaviour —
    /// main.rs's own "Gap 1" doc comment already states it always passes
    /// empty pivots right now. Wiring real pivots through therefore needs
    /// no signature change at all, only `main.rs` setting this field
    /// explicitly instead of leaving it defaulted; see this module's
    /// `encode_video` doc comment for the exact line to change.
    pub pivots: Vec<Pivot>,
}

impl Default for EncodeConfig {
    fn default() -> Self {
        Self {
            crf: 18,
            preset: "medium".to_string(),
            audio: AudioSource::Silent,
            out_name: "scrollcast.mp4".to_string(),
            pivots: Vec::new(),
        }
    }
}

/// What a finished (or attempted) encode produced, for a caller that wants to
/// report it rather than re-derive it by re-probing the output file.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodeReport {
    pub output_path: PathBuf,
    pub frame_count: u64,
    pub video_duration_secs: f64,
    /// `Some` only when `EncodeConfig::audio` was `File(..)` and probing its
    /// duration succeeded.
    pub audio_duration_secs: Option<f64>,
    /// Human-readable mismatch report, per the task's explicit instruction
    /// not to silently stretch/pad/truncate a supplied audio track: this is
    /// the "report the delta explicitly" output, not a correction.
    pub audio_delta_message: Option<String>,
}

/// Resolve the `ffmpeg` binary in the required order: an explicit
/// `$GV_SCROLLCAST_FFMPEG` override, then `PATH`, then the Playwright-bundled
/// fallback this box ships. Structured as a thin wrapper around
/// [`resolve_binary_with`] so the actual decision (which candidate wins) is
/// a pure function tested with fake inputs — this wrapper's only job is
/// supplying the real environment and a real filesystem check.
///
/// The fallback path is resolved from `$HOME` rather than hardcoded to a
/// specific home directory, since the version-pinned
/// `ms-playwright/ffmpeg-1011` directory name is itself something a future
/// Playwright upgrade will change — `PATH`/the env override are the two
/// resolution steps meant to survive that; this fallback is deliberately
/// last-resort and named for exactly the binary present when this module was
/// written.
pub fn resolve_ffmpeg() -> Result<PathBuf> {
    let fallback = default_ffmpeg_fallback();
    resolve_binary_with(
        "ffmpeg",
        std::env::var("GV_SCROLLCAST_FFMPEG").ok(),
        std::env::var("PATH").ok(),
        &fallback,
        |p: &Path| p.is_file(),
    )
}

fn default_ffmpeg_fallback() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache/ms-playwright/ffmpeg-1011/ffmpeg-linux")
}

/// The pure resolution decision behind [`resolve_ffmpeg`]: given an optional
/// override, an optional `PATH` string, a fallback, and a way to test
/// existence, pick the first candidate that exists, in that priority order.
/// Taking `exists` as a parameter (rather than calling `Path::is_file`
/// directly) is what lets the priority *order* be tested without touching
/// the real filesystem or environment.
pub(crate) fn resolve_binary_with(
    name: &str,
    env_override: Option<String>,
    path_var: Option<String>,
    fallback: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Result<PathBuf> {
    if let Some(p) = env_override {
        let path = PathBuf::from(&p);
        if exists(&path) {
            return Ok(path);
        }
        bail!(
            "${{GV_SCROLLCAST_{}}}={p} does not point to an existing file",
            name.to_uppercase()
        );
    }
    if let Some(path_var) = path_var {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(name);
            if exists(&candidate) {
                return Ok(candidate);
            }
        }
    }
    if exists(fallback) {
        return Ok(fallback.to_path_buf());
    }
    bail!(
        "{name} not found: checked $GV_SCROLLCAST_{} (unset or invalid), every directory on \
         $PATH, and the bundled fallback at {} — none exist",
        name.to_uppercase(),
        fallback.display(),
    );
}

/// Run `ffmpeg` with the given args and capture stdout+stderr as UTF-8,
/// regardless of exit status. Used for the small, finite probe calls
/// (`-encoders`, `-decoders`, `-muxers`, and audio-duration probing) where
/// the output is a few KB of text and a non-piped-output deadlock risk does
/// not apply — contrast with the actual encode call in [`run_encode`], which
/// streams frames and needs the stderr-draining care documented there.
async fn run_capture(ffmpeg: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(ffmpeg)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to spawn {}", ffmpeg.display()))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(text)
}

/// Fail fast, before any frame is decoded or written, if the resolved
/// `ffmpeg` cannot do this module's job. See this module's top doc comment
/// ("Fail at startup, never mid-encode") for why this exists and exactly
/// which capability gap it was written to catch.
pub async fn check_ffmpeg_capabilities(ffmpeg: &Path) -> Result<()> {
    let encoders = run_capture(ffmpeg, &["-hide_banner", "-encoders"]).await?;
    let decoders = run_capture(ffmpeg, &["-hide_banner", "-decoders"]).await?;
    let muxers = run_capture(ffmpeg, &["-hide_banner", "-muxers"]).await?;
    let filters = run_capture(ffmpeg, &["-hide_banner", "-filters"]).await?;
    let missing = missing_capabilities(&encoders, &decoders, &muxers, &filters);
    if !missing.is_empty() {
        bail!(
            "ffmpeg at {} is missing required capabilities: {}. This crate needs an ffmpeg \
             build with libx264 + aac encoders, a png decoder, an mp4 muxer, and the lavfi \
             anullsrc source filter (used for the default silent-audio track). If this is the \
             Playwright-bundled ffmpeg, that build is deliberately stripped down for Playwright's \
             own webm/vp8 screen-recording use and lacks all of these; point \
             $GV_SCROLLCAST_FFMPEG at a full ffmpeg build instead.",
            ffmpeg.display(),
            missing.join(", "),
        );
    }
    Ok(())
}

/// Pure capability check: given the text of `-encoders`/`-decoders`/`-muxers`/
/// `-filters`, name every required capability that is absent. Matches on the
/// *name* column of each listing line (not a raw substring search of the
/// whole blob), so a codec's free-text description happening to mention
/// "aac" or "png" can never produce a false pass.
///
/// `anullsrc` gets its own check here rather than being assumed to come free
/// with `libx264`/`aac`/`png`/`mp4`: it is a distinct lavfi *filter*, gated
/// independently of codec/muxer support in a minimal build, and it is what
/// the *default* (no `--audio` flag) path depends on for its silent
/// placeholder track. Missing this check would mean the most common
/// invocation — the default one — could sail past this startup gate and
/// only fail once `run_encode` is already streaming frames, which is
/// exactly the "fails mid-encode instead of at startup" failure mode this
/// whole function exists to prevent.
pub(crate) fn missing_capabilities(
    encoders: &str,
    decoders: &str,
    muxers: &str,
    filters: &str,
) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if !listing_has_name(encoders, "libx264") {
        missing.push("encoder:libx264 (H.264)");
    }
    if !listing_has_name(encoders, "aac") {
        missing.push("encoder:aac");
    }
    if !listing_has_name(decoders, "png") {
        missing.push("decoder:png");
    }
    if !listing_has_name(muxers, "mp4") {
        missing.push("muxer:mp4");
    }
    if !listing_has_name(filters, "anullsrc") {
        missing.push("filter:anullsrc (lavfi, needed for the default silent audio track)");
    }
    missing
}

/// One `ffmpeg -encoders`/`-decoders`/`-muxers` listing line looks like
/// `" V..... libx264              libx264 H.264 / AVC / ..."` or
/// `"  E mp4             MP4 (MPEG-4 Part 14)"` — a flags token, then the
/// bare name, then a free-text description. `split_whitespace().nth(1)` gets
/// the name regardless of how many characters the flags token uses, which is
/// what keeps this immune to the description text incidentally containing
/// the name being searched for.
fn listing_has_name(listing: &str, name: &str) -> bool {
    listing
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(name))
}

/// Decode `png_path` to a flat RGB24 byte buffer via `ffmpeg`, once, in full.
/// See this module's top doc comment for why this is an `ffmpeg` subprocess
/// call rather than a Rust image-decoding crate, and why decoding once into
/// memory (rather than per-frame) is the affordable option at this box's
/// scale. `expected_width`/`expected_height` come from the capture stage
/// that produced this PNG (Chromium already reports them) rather than being
/// re-derived here by parsing ffmpeg's log text — this module treats them as
/// a given and only sanity-checks the decoded byte count against them.
pub async fn decode_png_to_rgb24(
    ffmpeg: &Path,
    png_path: &Path,
    expected_width: u32,
    expected_height: u32,
) -> Result<Vec<u8>> {
    let expected_bytes = expected_width as u64 * expected_height as u64 * 3;
    if expected_bytes > MAX_DECODE_BYTES {
        bail!(
            "source image {}x{} would decode to {expected_bytes} raw bytes, over this module's \
             {MAX_DECODE_BYTES}-byte guard — refusing rather than risking an OOM on an 8GB box \
             that also runs a live server; if a graph genuinely needs to be this tall, raise \
             MAX_DECODE_BYTES deliberately",
            expected_width,
            expected_height,
        );
    }

    let child = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(png_path)
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to spawn {} to decode {}",
                ffmpeg.display(),
                png_path.display()
            )
        })?;

    if !child.status.success() {
        bail!(
            "ffmpeg failed to decode {}: {}",
            png_path.display(),
            String::from_utf8_lossy(&child.stderr),
        );
    }

    let raw = child.stdout;
    if raw.len() as u64 != expected_bytes {
        bail!(
            "decoding {} produced {} raw bytes, expected {expected_bytes} for a {expected_width}x{expected_height} \
             rgb24 image — the capture stage's reported dimensions likely do not match the actual PNG",
            png_path.display(),
            raw.len(),
        );
    }
    Ok(raw)
}

/// Where the top-left corner of the camera's crop window sits, in whole
/// source-image pixel rows, for a raw `y` from [`camera_y_at`]. Clamped so
/// the window never reads past the bottom of the source image even if `y`
/// (which `camera_y_at` allows to reach `image_height` — see its own doc
/// comment) would otherwise put the *bottom* of a `viewport_h`-tall window
/// off the edge.
pub(crate) fn clamp_crop_y(y: f64, image_height: u32, viewport_h: u32) -> u32 {
    let max_y = image_height.saturating_sub(viewport_h);
    (y.max(0.0).round() as u32).min(max_y)
}

/// Slice exactly one camera frame's rows out of the decoded source buffer.
/// Zero-copy: because the source image's width is required to equal the
/// camera's width (checked here, not silently resized — see this module's
/// top doc comment on why a width mismatch is a hard error), every row is
/// contiguous in `raw_rgb` and a whole frame is therefore one contiguous
/// span, not `viewport_h` separate row copies.
pub(crate) fn extract_frame(
    raw_rgb: &[u8],
    image_width: u32,
    viewport_w: u32,
    viewport_h: u32,
    y_px: u32,
) -> Result<&[u8]> {
    if image_width != viewport_w {
        bail!(
            "captured image width {image_width}px does not match the {viewport_w}px camera; the \
             print-view capture stage is expected to render at exactly the camera's width, not be \
             scaled or padded to fit here"
        );
    }
    let row_bytes = image_width as usize * 3;
    let start = y_px as usize * row_bytes;
    let end = start + viewport_h as usize * row_bytes;
    raw_rgb.get(start..end).with_context(|| {
        format!(
            "frame crop at y={y_px} needs bytes [{start}..{end}) but the decoded buffer is only \
             {} bytes long — the image_height passed to encode_video likely does not match the \
             actual PNG",
            raw_rgb.len(),
        )
    })
}

/// Parse the `Duration: HH:MM:SS.ss` line ffmpeg prints to stderr while
/// probing any input, into seconds. Used to learn a supplied `--audio`
/// file's length without an `ffprobe` binary (not one of the binaries this
/// crate is guaranteed — see the task's binary list), since plain `ffmpeg -i`
/// prints exactly this line before erroring on the missing output — an error
/// we deliberately ignore, since we are asking `ffmpeg` to compute the
/// probe with the same input it will actually mux later, not to encode
/// anything on this call.
pub(crate) fn parse_ffmpeg_duration(stderr: &str) -> Option<f64> {
    let idx = stderr.find("Duration: ")?;
    let rest = &stderr[idx + "Duration: ".len()..];
    let end = rest.find(',').unwrap_or(rest.len());
    let ts = rest[..end].trim();
    let parts: Vec<&str> = ts.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: f64 = parts[0].parse().ok()?;
    let m: f64 = parts[1].parse().ok()?;
    let s: f64 = parts[2].parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

/// Probe an audio file's duration by asking `ffmpeg` to read (not convert)
/// it. Non-zero/error exit is expected and ignored here — see
/// `parse_ffmpeg_duration`'s doc comment for why.
pub async fn probe_audio_duration(ffmpeg: &Path, audio_path: &Path) -> Result<f64> {
    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(audio_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to spawn {} to probe {}",
                ffmpeg.display(),
                audio_path.display()
            )
        })?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_ffmpeg_duration(&stderr).with_context(|| {
        format!(
            "could not find a Duration: line in ffmpeg's probe of {} — is it a readable audio file?",
            audio_path.display(),
        )
    })
}

/// Build the "supplied audio doesn't match video length" report the task
/// requires in place of silently stretching, padding, or truncating either
/// stream. `None` when the two are within 50ms of each other (encoder frame
/// boundaries make exact equality unlikely even for intentionally-matched
/// inputs, so this is a "close enough to not be worth a report" tolerance,
/// not a correctness threshold).
pub(crate) fn audio_delta_message(video_secs: f64, audio_secs: f64) -> Option<String> {
    let delta = audio_secs - video_secs;
    if delta.abs() < 0.05 {
        return None;
    }
    if delta > 0.0 {
        Some(format!(
            "supplied audio is {delta:.2}s longer than the video ({audio_secs:.2}s audio vs \
             {video_secs:.2}s video) — muxed as-is, neither stream stretched, padded, or truncated"
        ))
    } else {
        Some(format!(
            "supplied audio is {:.2}s shorter than the video ({audio_secs:.2}s audio vs \
             {video_secs:.2}s video) — muxed as-is, neither stream stretched, padded, or truncated",
            -delta,
        ))
    }
}

/// Build the full `ffmpeg` argument list for the encode pass. Pure and
/// tested on its own (see the tests below) precisely because this is where
/// the yuv420p/determinism/faststart requirements actually live — asserting
/// on this function's output is what would catch a future edit "optimising"
/// any one of them away, without needing to run a real encode to notice.
pub(crate) fn build_encode_args(
    config: &EncodeConfig,
    out_path: &Path,
    video_duration_secs: f64,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        // Video: raw frames arrive on our own stdin. No `-t`/`-frames:v` needed
        // here — the input naturally ends exactly at frame_count when this
        // module closes stdin, so the video stream's length is exact by
        // construction, never by a separately-specified duration that could
        // drift out of sync with what was actually written.
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "rgb24".into(),
        "-s".into(),
        format!("{VIDEO_WIDTH}x{VIDEO_HEIGHT}"),
        "-r".into(),
        FPS.to_string(),
        "-i".into(),
        "pipe:0".into(),
    ];

    match &config.audio {
        AudioSource::Silent => {
            // `-t` here is an *input* option (it precedes this `-i`), so it
            // bounds only this lavfi source, which is otherwise an infinite
            // generator. This is constructing the placeholder track to the
            // video's exact length in the first place — not stretching or
            // truncating a real asset, which is the behaviour the task
            // reserves for a caller-supplied `--audio` file instead (see the
            // `File` arm below, which applies no such bound).
            args.extend([
                "-f".into(),
                "lavfi".into(),
                "-t".into(),
                format!("{video_duration_secs}"),
                "-i".into(),
                "anullsrc=channel_layout=stereo:sample_rate=48000".into(),
            ]);
        }
        AudioSource::File(path) => {
            args.push("-i".into());
            args.push(path.display().to_string());
        }
    }

    args.extend([
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "1:a:0".into(),
        // yuv420p: load-bearing, not cosmetic — see this module's top doc
        // comment. Do not "simplify" this away.
        "-vf".into(),
        "format=yuv420p".into(),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        config.preset.clone(),
        "-crf".into(),
        config.crf.to_string(),
        // Determinism: see this module's top doc comment for exactly what
        // this combination does and does not guarantee.
        "-threads".into(),
        "1".into(),
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        "128k".into(),
        "-fflags".into(),
        "+bitexact".into(),
        "-flags:v".into(),
        "+bitexact".into(),
        "-flags:a".into(),
        "+bitexact".into(),
        "-map_metadata".into(),
        "-1".into(),
        "-metadata".into(),
        "creation_time=1970-01-01T00:00:00Z".into(),
        // +faststart: moves the MP4 moov atom to the front of the file so a
        // player (or a `<video>` tag) can start playback/seek before the
        // whole file has downloaded — the task's "streams/seeks without a
        // full download" requirement.
        "-movflags".into(),
        "+faststart".into(),
        out_path.display().to_string(),
    ]);

    args
}

/// Run the built encode command, streaming frames into its stdin. Unlike
/// [`run_capture`], this spawns with stdin piped (we write to it) and stderr
/// piped and drained *concurrently* with those writes via
/// [`tokio::task::spawn`] — ffmpeg's own stderr carries continuous progress
/// output for a multi-minute encode, and without a concurrent drain the OS
/// pipe buffer fills, ffmpeg blocks trying to flush it, and this module's
/// `write_all` calls to stdin block right back waiting for ffmpeg to resume
/// reading — a classic two-pipe deadlock. Video output goes to a real file
/// path (an `ffmpeg` argument, not a pipe), so stdout needs no draining here.
async fn run_encode(ffmpeg: &Path, args: &[String], frames: FrameIter<'_>) -> Result<()> {
    let mut child = Command::new(ffmpeg)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {}", ffmpeg.display()))?;

    let mut stdin = child
        .stdin
        .take()
        .context("ffmpeg child had no stdin handle")?;
    let mut stderr = child
        .stderr
        .take()
        .context("ffmpeg child had no stderr handle")?;

    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });

    let mut write_err = None;
    for frame in frames {
        // `&frame` (`&Cow<[u8]>`) deref-coerces to `&[u8]` here, the same
        // mechanism `&String` -> `&str` uses — `write_all` sees the same
        // borrowed bytes whether this frame was a zero-copy scroll slice or
        // an owned, card-composited dwell frame (see `FrameIter::Item`'s doc
        // comment).
        if let Err(e) = stdin.write_all(&frame).await {
            write_err = Some(e);
            break;
        }
    }
    drop(stdin); // signal EOF so ffmpeg's rawvideo input ends

    let status = child
        .wait()
        .await
        .context("failed to wait on ffmpeg child")?;
    let stderr_text = stderr_task.await.unwrap_or_default();
    let stderr_text = String::from_utf8_lossy(&stderr_text);

    if let Some(e) = write_err {
        bail!("writing frames to ffmpeg's stdin failed: {e}\nffmpeg stderr:\n{stderr_text}");
    }
    if !status.success() {
        bail!("ffmpeg exited with {status}\nffmpeg stderr:\n{stderr_text}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pivot callout cards (#325 review finding A)
// ---------------------------------------------------------------------------
//
// The owner's ask, verbatim from the review: at a pivot the scroll pauses,
// "a little window that explains what was going on," then resumes. Before
// this section existed, a dwell (`Segment::is_dwell()`, pacing.rs:55-59) was
// a silent frozen frame — `encode.rs` never imported `Pivot` and had no text
// rendering at all, so the callout the owner asked for simply never
// appeared. This section is that card.
//
// Where the two inputs come from: `Segment::is_dwell()` is pacing.rs's own
// method (pacing.rs:56-58, `y_start == y_end`); the `Pivot` text
// (`label`/`detail`) is assembled by `chapters::render_label`/
// `render_detail` (chapters.rs:267-315) from a commit's refs/merge-ness/
// message, already capped to `chapters::CARD_MAX_WORDS` (12) words
// (chapters.rs:301). Neither reaches this file today: `main.rs` currently
// always calls `chapters::detect_pivots` with empty input (main.rs:442,
// its own documented "Gap 1"), so `pivots` is always `vec![]`. Wiring real
// pivots through therefore does not need a change to this crate's control
// flow, only two things: (1) this section, so a non-empty `pivots` list has
// somewhere to go, and (2) `main.rs` setting `EncodeConfig::pivots`
// explicitly — see `EncodeConfig::pivots`'s own doc comment for exactly why
// this is a new *struct field* rather than a new *function parameter* on
// [`encode_video`], and exactly which line in `main.rs` needs to change.
//
// ## Why compositing into the RGB buffer, not an ffmpeg `drawtext` filter
//
// The review names both options and prefers this one; the reasons given are
// worth restating because they are exactly why no `fontconfig`/`libfreetype`
// dependency shows up anywhere in this change. `drawtext` needs a font file
// resolvable at run time (`fontconfig`, or a hardcoded `fontfile=` path) —
// exactly the kind of environment-dependent behaviour this module's own top
// doc comment (`check_ffmpeg_capabilities`, "Fail at startup, never
// mid-encode") exists to design out of this crate. Drawing into the buffer
// this module already owns needs no new capability probe, no new binary
// dependency, and produces the exact same bytes on every machine that runs
// the same pinned ffmpeg — consistent with this module's existing
// determinism story (see the top doc comment's "Determinism" section).
// It is also the only one of the two options this module can unit-test
// without spawning ffmpeg at all: a card is asserted by diffing byte buffers,
// not by parsing rendered video output.
//
// ## The font: a tiny fixed 5x7 bitmap, uppercase-only, by design
//
// No Rust font-rendering crate is added, for the same reason `encode.rs`
// shells out to `ffmpeg` for PNG decoding rather than adding an image crate
// (see this file's top doc comment) — an owned dependency here would be a
// cross-lane addition to `Cargo.toml`, outside this lane's file set.
// Instead: a 5-column x 7-row bitmap glyph per character, packed one `u8`
// per row (bit `c`, `c` in `0..5`, set when column `c` is lit — bit 0 is the
// LEFTMOST pixel, not the usual "bit 0 is least significant / rightmost"
// convention, so `draw_glyph_clamped` shifts by the column index directly).
// The 48 glyphs in `glyph_rows` below were transcribed from ASCII art by a
// small Python script (kept in this task's scratchpad, not part of the
// crate) rather than hand-counted into hex/decimal — hand-transcribing 48
// glyphs' worth of bit patterns is exactly the kind of mechanical task a
// human silently gets a few bits wrong on, and a wrong bit here is a
// wrong-looking letter that a `cargo test` run cannot catch by itself
// (which is why `card_pixels_are_confined_to_the_card_region_not_the_frame`
// and friends below assert on *regions*, not on font content — this
// module's tests cannot verify a glyph "looks like" its letter, only that
// drawing stays where it's supposed to and something actually changed).
//
// Deliberately uppercase-only: rendering both cases in a legible 5x7 cell
// needs a materially bigger glyph table (roughly double: 26 more letters,
// several of which — g/j/p/q/y — need descenders a 7-row cell cannot show
// without shrinking the cap-height letters to make room), for a card whose
// entire job is a ~12-word caption on screen for 3 seconds
// (`pacing::DEFAULT_DWELL_SECS`) — legibility of case, not preservation of
// it, is what that budget buys. `prepare_card_text` upper-cases (and
// substitutes a couple of Unicode punctuation marks this crate's own text
// actually produces — see its doc comment) before any glyph lookup runs.
// Any character with no glyph (accented letters, most Unicode punctuation,
// `chapters::cap_words`'s `…` before substitution) renders as a blank cell
// rather than a placeholder box or a panic — a card that quietly drops one
// unsupported glyph is honest about a known font-scope limit; a
// crash on a tag name with an unexpected character would not be.

/// Bitmap glyph width/height, in source pixels before `scale` is applied —
/// see this section's module doc comment for the row/bit convention.
const GLYPH_WIDTH: u32 = 5;
const GLYPH_HEIGHT: u32 = 7;

/// Look up one character's 7-row bitmap. Falls back to a blank (all-zero)
/// glyph — rendered as empty space — for anything not in this crate's
/// deliberately small uppercase-plus-punctuation set; see this section's
/// module doc comment for why that is the right failure mode here (a
/// dropped glyph, never a panic, on a tag/branch name this crate does not
/// control the character set of).
fn glyph_rows(c: char) -> [u8; 7] {
    match c {
        ' ' => [0, 0, 0, 0, 0, 0, 0],
        '!' => [4, 4, 4, 4, 4, 0, 4],
        '\'' => [4, 4, 0, 0, 0, 0, 0],
        '(' => [8, 4, 2, 2, 2, 4, 8],
        ')' => [2, 4, 8, 8, 8, 4, 2],
        ',' => [0, 0, 0, 0, 0, 4, 2],
        '-' => [0, 0, 0, 31, 0, 0, 0],
        '.' => [0, 0, 0, 0, 0, 6, 6],
        '/' => [16, 8, 8, 4, 2, 2, 1],
        '0' => [14, 17, 25, 21, 19, 17, 14],
        '1' => [4, 6, 4, 4, 4, 4, 31],
        '2' => [14, 17, 16, 8, 4, 2, 31],
        '3' => [15, 16, 16, 12, 16, 16, 15],
        '4' => [8, 12, 10, 9, 31, 8, 8],
        '5' => [31, 1, 15, 16, 16, 16, 15],
        '6' => [12, 2, 1, 15, 17, 17, 14],
        '7' => [31, 16, 8, 4, 2, 2, 2],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 30, 16, 8, 6],
        ':' => [0, 4, 0, 0, 0, 4, 0],
        '?' => [14, 17, 16, 8, 4, 0, 4],
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'B' => [15, 17, 17, 15, 17, 17, 15],
        'C' => [30, 1, 1, 1, 1, 1, 30],
        'D' => [15, 17, 17, 17, 17, 17, 15],
        'E' => [31, 1, 1, 15, 1, 1, 31],
        'F' => [31, 1, 1, 15, 1, 1, 1],
        'G' => [30, 1, 1, 29, 17, 17, 30],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [31, 4, 4, 4, 4, 4, 31],
        'J' => [28, 8, 8, 8, 8, 9, 6],
        'K' => [17, 9, 5, 3, 5, 9, 17],
        'L' => [1, 1, 1, 1, 1, 1, 31],
        'M' => [17, 27, 21, 17, 17, 17, 17],
        'N' => [17, 19, 21, 25, 17, 17, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [15, 17, 17, 15, 1, 1, 1],
        'Q' => [14, 17, 17, 17, 21, 9, 22],
        'R' => [15, 17, 17, 15, 5, 9, 17],
        'S' => [30, 1, 1, 14, 16, 16, 15],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'V' => [17, 17, 17, 17, 17, 10, 4],
        'W' => [17, 17, 17, 21, 21, 27, 17],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        'Z' => [31, 16, 8, 4, 2, 1, 31],
        '_' => [0, 0, 0, 0, 0, 0, 31],
        _ => [0, 0, 0, 0, 0, 0, 0],
    }
}

/// Upper-case the text and fold in the two non-ASCII punctuation marks this
/// crate's own callout text can actually contain, before any glyph lookup:
/// `chapters::render_detail` (chapters.rs:303-315) always joins with an em
/// dash (`" — "`), and `chapters::cap_words` (chapters.rs:321-327) appends
/// `"…"` (a single Unicode ellipsis character, not three periods) when it
/// truncates. Neither has a glyph in this crate's deliberately small font
/// (see this section's module doc comment), so both are rewritten to their
/// nearest ASCII punctuation here rather than silently rendering as blank
/// cells — a truncated detail line should still visibly end in *something*
/// that reads as "more was cut here."
fn prepare_card_text(s: &str) -> String {
    s.replace('…', "...")
        .replace(['—', '–'], "-")
        .to_uppercase()
}

/// A frame buffer plus a fixed clip rectangle, bundled so every drawing
/// primitive below (`set_pixel`/`fill_rect`/`draw_glyph`/`draw_text_line`)
/// takes one fewer argument than a bare-function version would — the
/// alternative (threading `frame`, `frame_width`, and `clip` through each
/// free function individually) is what tripped clippy's `too_many_arguments`
/// lint before this refactor, for functions that were correct but simply had
/// more parameters than the lint's default threshold. Every one of these
/// methods still routes through `set_pixel`, which is what makes "drawing a
/// card never touches a pixel outside `clip`" a property of this struct's
/// contract rather than something each caller has to get right
/// independently — see `draw_pivot_card`, the one place `clip` is ever set
/// to something other than the whole frame.
struct ClippedCanvas<'a> {
    frame: &'a mut [u8],
    frame_width: u32,
    /// `(x_min, y_min, x_max_exclusive, y_max_exclusive)`.
    clip: (u32, u32, u32, u32),
}

impl ClippedCanvas<'_> {
    /// Set one pixel to `rgb`, but only if `(x, y)` falls inside `self.clip`.
    /// Also defends against `frame` being shorter than `frame_width * some
    /// height` implies (defensive only; `encode_video`'s frame buffers are
    /// always exactly one full `VIDEO_WIDTH x VIDEO_HEIGHT` frame, but a
    /// bounds-checked write costs nothing here and this method has no other
    /// way to learn the buffer's real height).
    fn set_pixel(&mut self, x: u32, y: u32, rgb: [u8; 3]) {
        let (x_min, y_min, x_max, y_max) = self.clip;
        if x < x_min || x >= x_max || y < y_min || y >= y_max {
            return;
        }
        let idx = (y as usize * self.frame_width as usize + x as usize) * 3;
        if let Some(px) = self.frame.get_mut(idx..idx + 3) {
            px.copy_from_slice(&rgb);
        }
    }

    /// Fill a `w x h` rectangle at `(x0, y0)` with `rgb`, clipped to
    /// `self.clip`. Used both for the card's background/border (a big solid
    /// rect) and for each lit font pixel scaled up to a `scale x scale`
    /// block (see `draw_glyph`) — a "filled rectangle" is the one primitive
    /// both of those are built from.
    fn fill_rect(&mut self, x0: u32, y0: u32, w: u32, h: u32, rgb: [u8; 3]) {
        for y in y0..y0.saturating_add(h) {
            for x in x0..x0.saturating_add(w) {
                self.set_pixel(x, y, rgb);
            }
        }
    }

    /// Draw one glyph at `(x0, y0)` (its top-left corner), each of its 5x7
    /// source pixels scaled up to a `scale x scale` block so a tiny bitmap
    /// font is legible on a 1920x1080 frame — see `draw_pivot_card`'s doc
    /// comment for the actual scale factors this crate uses for the label
    /// vs. detail lines.
    fn draw_glyph(&mut self, x0: u32, y0: u32, scale: u32, rows: [u8; 7], rgb: [u8; 3]) {
        for (row_idx, row_bits) in rows.iter().enumerate() {
            for col in 0..GLYPH_WIDTH {
                if (row_bits >> col) & 1 == 1 {
                    let px = x0 + col * scale;
                    let py = y0 + row_idx as u32 * scale;
                    self.fill_rect(px, py, scale, scale, rgb);
                }
            }
        }
    }

    /// Draw a whole line of text left-to-right starting at `(x0, y0)`, one
    /// `GLYPH_WIDTH + 1` (a 1-source-pixel gap between glyphs, scaled the
    /// same as the glyphs themselves) column pitch per character. No line
    /// wrapping — a line that runs past `self.clip`'s right edge is
    /// silently cut off there (every pixel past it is out of `clip` and
    /// `set_pixel` drops it), rather than overflowing onto the rest of the
    /// frame or onto a second row. This is deliberate: `chapters::
    /// cap_words` already bounds the detail line's word count, and a card
    /// whose label happens to come from an unusually long tag/branch name
    /// should crop cleanly at its own edge, not bleed pixels into the
    /// scrolling graph behind it.
    fn draw_text_line(&mut self, x0: u32, y0: u32, scale: u32, text: &str, rgb: [u8; 3]) {
        let pitch = (GLYPH_WIDTH + 1) * scale;
        let mut x = x0;
        for c in text.chars() {
            self.draw_glyph(x, y0, scale, glyph_rows(c), rgb);
            x += pitch;
        }
    }
}

/// The card's fixed geometry, in `VIDEO_WIDTH x VIDEO_HEIGHT` frame pixels.
/// Fixed rather than configurable: this is a repair-pass addition closing a
/// blocker finding, not a new CLI surface, and a fixed layout is what keeps
/// `card_pixels_are_confined_to_the_card_region_not_the_frame` (below) a
/// meaningful test rather than one that has to re-derive the geometry it's
/// checking.
const CARD_WIDTH: u32 = 1200;
const CARD_HEIGHT: u32 = 140;
const CARD_MARGIN_BOTTOM: u32 = 90;
const CARD_X: u32 = (VIDEO_WIDTH - CARD_WIDTH) / 2;
const CARD_Y: u32 = VIDEO_HEIGHT - CARD_MARGIN_BOTTOM - CARD_HEIGHT;
const CARD_BORDER_PX: u32 = 1;
const CARD_PADDING_X: u32 = 28;
const CARD_PADDING_TOP: u32 = 24;
const CARD_LABEL_SCALE: u32 = 4;
const CARD_DETAIL_SCALE: u32 = 2;
const CARD_LINE_GAP: u32 = 14;

const CARD_BORDER_RGB: [u8; 3] = [225, 225, 230];
const CARD_BG_RGB: [u8; 3] = [18, 22, 34];
const CARD_LABEL_RGB: [u8; 3] = [255, 255, 255];
const CARD_DETAIL_RGB: [u8; 3] = [195, 200, 210];

/// Composite the pivot callout card onto one frame buffer, in place. `frame`
/// must be exactly one `VIDEO_WIDTH x VIDEO_HEIGHT` rgb24 frame (the same
/// shape `extract_frame` produces) — this function has no way to check that
/// beyond the bounds checks already inside `set_pixel_clamped`.
///
/// Layout: a filled rect + 1px border (the review's own suggested shape),
/// bottom-centered with a `CARD_MARGIN_BOTTOM`-px gap from the frame's
/// bottom edge, holding two left-aligned lines of text — the label at 4x
/// scale, the detail line at 2x scale below it. Every pixel this function
/// touches is inside `[CARD_X, CARD_X + CARD_WIDTH) x [CARD_Y, CARD_Y +
/// CARD_HEIGHT)` by construction (that rectangle is `clip`, below, and every
/// drawing call in this function routes through it) — see
/// `card_pixels_are_confined_to_the_card_region_not_the_frame` for the test
/// that pins this down.
pub(crate) fn draw_pivot_card(frame: &mut [u8], label: &str, detail: &str) {
    let clip = (CARD_X, CARD_Y, CARD_X + CARD_WIDTH, CARD_Y + CARD_HEIGHT);
    let mut canvas = ClippedCanvas {
        frame,
        frame_width: VIDEO_WIDTH,
        clip,
    };

    // Border, then an inset background fill on top — the "filled rect + 1px
    // border" the review asked for, built from the one rectangle primitive
    // both need rather than a separate stroke-only border routine.
    canvas.fill_rect(CARD_X, CARD_Y, CARD_WIDTH, CARD_HEIGHT, CARD_BORDER_RGB);
    canvas.fill_rect(
        CARD_X + CARD_BORDER_PX,
        CARD_Y + CARD_BORDER_PX,
        CARD_WIDTH - 2 * CARD_BORDER_PX,
        CARD_HEIGHT - 2 * CARD_BORDER_PX,
        CARD_BG_RGB,
    );

    let label_text = prepare_card_text(label);
    let detail_text = prepare_card_text(detail);
    let label_y = CARD_Y + CARD_PADDING_TOP;
    let detail_y = label_y + GLYPH_HEIGHT * CARD_LABEL_SCALE + CARD_LINE_GAP;

    canvas.draw_text_line(
        CARD_X + CARD_PADDING_X,
        label_y,
        CARD_LABEL_SCALE,
        &label_text,
        CARD_LABEL_RGB,
    );
    canvas.draw_text_line(
        CARD_X + CARD_PADDING_X,
        detail_y,
        CARD_DETAIL_SCALE,
        &detail_text,
        CARD_DETAIL_RGB,
    );
}

/// Match each dwell segment to the `Pivot` whose text it should show while
/// the scroll holds. Mirrors `chapters::format_chapters`'s own matching rule
/// (`seg.is_dwell() && seg.y_start == pivot.y`, chapters.rs:378-382) rather
/// than inventing a second one — both are answering the same question
/// ("which pivot does this dwell segment belong to?") from the same two
/// inputs, and a caller passing pivots straight from
/// `chapters::detect_pivots` into both `format_chapters` and this crate's
/// `EncodeConfig::pivots` needs both answers to agree.
///
/// Computed once, up front, one entry per segment (not per frame): a dwell
/// can hold for `pacing::DEFAULT_DWELL_SECS` (3.0s) at `FPS` (30), i.e. ~90
/// frames, and every one of them needs the same answer — there is no reason
/// to re-walk `pivots` 90 times to get it.
fn build_dwell_pivot_text(segments: &[Segment], pivots: &[Pivot]) -> Vec<Option<(String, String)>> {
    segments
        .iter()
        .map(|seg| {
            if !seg.is_dwell() {
                return None;
            }
            pivots
                .iter()
                .find(|p| p.y == seg.y_start)
                .map(|p| (p.label.clone(), p.detail.clone()))
        })
        .collect()
}

/// Which segment (by index) covers time `t` — mirrors `pacing::camera_y_at`'s
/// own walk over `segments` (pacing.rs:220-234) step for step, but returns
/// the segment's *index* rather than its interpolated `y`. `camera_y_at`
/// has no reason to expose that index (its only job is a position), but
/// `FrameIter` needs it to look up `build_dwell_pivot_text`'s precomputed
/// per-segment answer for the same time `t` it already asked `camera_y_at`
/// about.
///
/// Duplicated here rather than added to `pacing.rs` because `pacing.rs` is
/// out of this lane's file set for #325's repair pass (see the review's own
/// instruction: "except where a lane is EXPLICITLY told its caller may pass
/// different arguments to it — the file itself stays unedited"). If a
/// future change legitimately touches both files, consider hoisting this
/// back into `pacing.rs` as a `camera_segment_at` sibling to `camera_y_at`
/// so the two walks cannot silently drift apart.
fn segment_index_at(segments: &[Segment], t: f64) -> Option<usize> {
    let mut elapsed = 0.0_f64;
    for (i, seg) in segments.iter().enumerate() {
        let seg_end = elapsed + seg.duration_secs;
        if t <= seg_end || seg.duration_secs == 0.0 {
            return Some(i);
        }
        elapsed = seg_end;
    }
    if segments.is_empty() {
        None
    } else {
        Some(segments.len() - 1)
    }
}

/// Lazily yields each output frame's byte in order (a plain slice for a
/// scroll frame, an owned composited copy for a dwell frame with a pivot
/// card — see `Item`'s doc comment), computing its source `y` from
/// `camera_y_at` on demand rather than precomputing and storing every
/// frame's slice up front — with 7,200 frames that would be a 7,200-element
/// `Vec<_>`, cheap in itself, but there is no reason to pay even that when a
/// plain iterator does the same job.
struct FrameIter<'a> {
    raw_rgb: &'a [u8],
    segments: &'a [Segment],
    /// One entry per `segments` element, precomputed once by
    /// `build_dwell_pivot_text` before this iterator is built — see that
    /// function's doc comment for why this is computed up front rather than
    /// re-matched on every one of a dwell's ~90 frames.
    dwell_pivot_text: &'a [Option<(String, String)>],
    image_width: u32,
    image_height: u32,
    frame_count: u64,
    next: u64,
}

impl<'a> Iterator for FrameIter<'a> {
    /// Borrowed (zero-copy) for the common case — a scroll frame is exactly
    /// the slice `extract_frame` already produces, and this crate's whole
    /// piping design (see the top doc comment's "Frame delivery" section)
    /// depends on that staying zero-copy for the ~7,200 frames of a typical
    /// run. Owned only for the rare dwell-with-a-pivot-card frame (at most a
    /// few hundred per run — `chapters::detect_pivots`'s own `max_pivots`
    /// cap, times ~90 frames each), where `draw_pivot_card` must mutate a
    /// copy: `raw_rgb` is the one decoded source buffer every frame in the
    /// whole video borrows from (see `decode_png_to_rgb24`'s doc comment),
    /// so drawing directly into `self.raw_rgb`'s bytes would permanently
    /// burn the card into that shared buffer for every later frame that
    /// happens to read the same rows, not just the dwell frames it belongs
    /// to.
    type Item = Cow<'a, [u8]>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.frame_count {
            return None;
        }
        let t = self.next as f64 / FPS as f64;
        let y = camera_y_at(self.segments, t);
        let y_px = clamp_crop_y(y, self.image_height, VIDEO_HEIGHT);
        self.next += 1;
        // A slice-extraction failure here means the caller's image_width/
        // image_height do not match the decoded buffer — that is checked
        // once, up front, in `encode_video` before this iterator is ever
        // built, so by the time frames are being streamed this cannot fail.
        // `expect` is deliberate: this is an invariant this module itself
        // established a few lines above, not a fallible external input.
        let base = extract_frame(
            self.raw_rgb,
            self.image_width,
            VIDEO_WIDTH,
            VIDEO_HEIGHT,
            y_px,
        )
        .expect("frame bounds already validated in encode_video");

        // `segment_index_at` re-walks `segments` with the *same* `t` used
        // above for `camera_y_at`, so the two agree on which segment this
        // frame belongs to by construction (both are the same walk over the
        // same input) — see that function's doc comment for why this is a
        // deliberate duplication of `camera_y_at`'s logic rather than a
        // second, different rule.
        let dwell_text = segment_index_at(self.segments, t)
            .and_then(|idx| self.dwell_pivot_text.get(idx))
            .and_then(|entry| entry.as_ref());

        match dwell_text {
            Some((label, detail)) => {
                let mut composited = base.to_vec();
                draw_pivot_card(&mut composited, label, detail);
                Some(Cow::Owned(composited))
            }
            None => Some(Cow::Borrowed(base)),
        }
    }
}

/// The whole Lane 2 job: turn a rendered print-view PNG plus a pacing
/// timeline into an MP4 under `./out/`. See this module's top doc comment
/// for the reasoning behind every major decision inside this function; this
/// function's own body is deliberately thin glue over the pieces documented
/// above.
///
/// `image_width`/`image_height` are the capture stage's own record of the
/// PNG's dimensions (Chromium reports them directly when it renders the
/// page) — passed in rather than re-derived here, per this crate's standing
/// rule against re-deriving a value another stage already established.
pub async fn encode_video(
    source_png: &Path,
    image_width: u32,
    image_height: u32,
    segments: &[Segment],
    config: &EncodeConfig,
) -> Result<EncodeReport> {
    if config.out_name.contains(['/', '\\']) || config.out_name == ".." {
        bail!(
            "EncodeConfig::out_name must be a bare filename, got {:?} — encode_video only ever \
             writes under ./out/, and a path-like name could escape that",
            config.out_name,
        );
    }

    let ffmpeg = resolve_ffmpeg()?;
    check_ffmpeg_capabilities(&ffmpeg).await?;

    std::fs::create_dir_all("out").context("failed to create ./out/ output directory")?;
    let out_path = Path::new("out").join(&config.out_name);

    let video_duration_secs = total_duration(segments);
    let frame_count = (video_duration_secs * FPS as f64).round() as u64;
    if frame_count == 0 {
        bail!("timeline has zero total duration ({video_duration_secs}s) — nothing to encode");
    }

    let raw_rgb = decode_png_to_rgb24(&ffmpeg, source_png, image_width, image_height).await?;
    // Validate crop bounds once, up front, at y=0 — the smallest possible
    // crop window and therefore sufficient on its own: every later frame's
    // y comes from `clamp_crop_y`, whose `saturating_sub` guarantees it can
    // never place a window's bottom edge past `image_height` once a y=0
    // window already fits, so checking frame 0 here covers every frame
    // `FrameIter` will ever produce (see its doc comment) without re-checking
    // per frame.
    extract_frame(&raw_rgb, image_width, VIDEO_WIDTH, VIDEO_HEIGHT, 0)
        .context("source image is too short for even one camera frame")?;

    let (audio_duration_secs, audio_delta_message) = match &config.audio {
        AudioSource::Silent => (None, None),
        AudioSource::File(path) => {
            let audio_secs = probe_audio_duration(&ffmpeg, path).await?;
            let message = audio_delta_message(video_duration_secs, audio_secs);
            (Some(audio_secs), message)
        }
    };

    let args = build_encode_args(config, &out_path, video_duration_secs);
    // See `EncodeConfig::pivots`'s doc comment for why this comes from a
    // config field rather than a new `encode_video` parameter: `main.rs`
    // (this function's only caller) is out of this lane's file set, and a
    // struct field is the one way to thread this through that does not
    // require a call-site change there to keep compiling.
    let dwell_pivot_text = build_dwell_pivot_text(segments, &config.pivots);
    let frames = FrameIter {
        raw_rgb: &raw_rgb,
        segments,
        dwell_pivot_text: &dwell_pivot_text,
        image_width,
        image_height,
        frame_count,
        next: 0,
    };
    run_encode(&ffmpeg, &args, frames).await?;

    Ok(EncodeReport {
        output_path: out_path,
        frame_count,
        video_duration_secs,
        audio_duration_secs,
        audio_delta_message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- resolve_binary_with ----

    #[test]
    fn env_override_wins_when_it_exists() {
        let result = resolve_binary_with(
            "ffmpeg",
            Some("/override/ffmpeg".to_string()),
            Some("/usr/bin".to_string()),
            Path::new("/fallback/ffmpeg"),
            |p| p == Path::new("/override/ffmpeg") || p == Path::new("/usr/bin/ffmpeg"),
        )
        .unwrap();
        assert_eq!(result, PathBuf::from("/override/ffmpeg"));
    }

    #[test]
    fn env_override_that_does_not_exist_is_a_hard_error_not_a_silent_fallthrough() {
        // Mutation this catches: treating a bad override as "not set" and
        // quietly falling through to PATH/fallback, which would hide a typo
        // in $GV_SCROLLCAST_FFMPEG behind an unrelated binary succeeding.
        let result = resolve_binary_with(
            "ffmpeg",
            Some("/typo/ffmpeg".to_string()),
            Some("/usr/bin".to_string()),
            Path::new("/fallback/ffmpeg"),
            |p| p == Path::new("/usr/bin/ffmpeg") || p == Path::new("/fallback/ffmpeg"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn path_is_checked_before_the_fallback() {
        let result = resolve_binary_with(
            "ffmpeg",
            None,
            Some("/usr/bin:/opt/bin".to_string()),
            Path::new("/fallback/ffmpeg"),
            |p| p == Path::new("/opt/bin/ffmpeg") || p == Path::new("/fallback/ffmpeg"),
        )
        .unwrap();
        assert_eq!(result, PathBuf::from("/opt/bin/ffmpeg"));
    }

    #[test]
    fn fallback_is_used_only_when_nothing_else_exists() {
        let result = resolve_binary_with(
            "ffmpeg",
            None,
            Some("/usr/bin".to_string()),
            Path::new("/fallback/ffmpeg"),
            |p| p == Path::new("/fallback/ffmpeg"),
        )
        .unwrap();
        assert_eq!(result, PathBuf::from("/fallback/ffmpeg"));
    }

    #[test]
    fn nothing_existing_anywhere_is_a_clear_error() {
        let result = resolve_binary_with(
            "ffmpeg",
            None,
            Some("/usr/bin".to_string()),
            Path::new("/fallback/ffmpeg"),
            |_| false,
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("ffmpeg"),
            "error should name the binary: {msg}"
        );
        assert!(
            msg.contains("/fallback/ffmpeg"),
            "error should name the fallback path checked: {msg}"
        );
    }

    // ---- missing_capabilities / listing_has_name ----

    const SAMPLE_ENCODERS_FULL: &str = " V..... libx264              libx264 H.264 / AVC / MPEG-4 AVC\n A..... aac                  AAC (Advanced Audio Coding)\n";
    const SAMPLE_DECODERS_FULL: &str =
        " V....D png                  PNG (Portable Network Graphics) image\n";
    const SAMPLE_MUXERS_FULL: &str = "  E mp4             MP4 (MPEG-4 Part 14)\n";
    const SAMPLE_FILTERS_FULL: &str =
        " ... anullsrc          |->A       Null audio source, return empty audio frames.\n";

    #[test]
    fn a_fully_capable_ffmpeg_reports_nothing_missing() {
        let missing = missing_capabilities(
            SAMPLE_ENCODERS_FULL,
            SAMPLE_DECODERS_FULL,
            SAMPLE_MUXERS_FULL,
            SAMPLE_FILTERS_FULL,
        );
        assert!(missing.is_empty(), "unexpected missing: {missing:?}");
    }

    #[test]
    fn the_stripped_playwright_ffmpeg_is_correctly_detected_as_incapable() {
        // The exact shape this module was written to catch: real
        // `-encoders`/`-decoders`/`-muxers`/`-filters` output from the
        // Playwright ffmpeg bundle, which has none of the five required
        // capabilities — including `-filters`, which lists `crop`/`scale`/
        // `pad`/`format`/`abuffer` etc. (it needs those for its own
        // screenshot-to-webm pipeline) but no `anullsrc`, since it never
        // needs to synthesize audio.
        let encoders = " VF...D png                  PNG (Portable Network Graphics) image\n V..... libvpx               libvpx VP8 (codec vp8)\n";
        let decoders = " V....D mjpeg                MJPEG (Motion JPEG)\n";
        let muxers = "  E  image2          image2 sequence\n  E  webm            WebM\n";
        let filters = " ..C crop              V->V       Crop the input video.\n ... scale             V->V       Scale the input video size and/or convert the image format.\n ... abuffer           |->A       Buffer audio frames, and make them accessible to the filterchain.\n";
        let missing = missing_capabilities(encoders, decoders, muxers, filters);
        assert_eq!(
            missing.len(),
            5,
            "expected all five capabilities missing, got {missing:?}"
        );
    }

    #[test]
    fn a_description_mentioning_the_name_does_not_produce_a_false_pass() {
        // Mutation this catches: naive substring search over the whole
        // listing instead of matching the name column specifically. A
        // description that happens to say "aac" would otherwise satisfy
        // the check even with no aac encoder actually present.
        let encoders =
            " A..... libmp3lame           MP3 (some builds describe this as an aac alternative)\n";
        assert!(!listing_has_name(encoders, "aac"));
    }

    #[test]
    fn missing_libx264_alone_is_reported_precisely() {
        let encoders = " A..... aac                  AAC (Advanced Audio Coding)\n";
        let missing = missing_capabilities(
            encoders,
            SAMPLE_DECODERS_FULL,
            SAMPLE_MUXERS_FULL,
            SAMPLE_FILTERS_FULL,
        );
        assert_eq!(missing, vec!["encoder:libx264 (H.264)"]);
    }

    #[test]
    fn a_build_with_every_codec_but_no_lavfi_anullsrc_is_still_caught_at_startup() {
        // The gap this check exists to close: libx264/aac/png/mp4 are all
        // present (so a codec-only probe would pass), but `anullsrc` — a
        // separate lavfi filter the *default* no-`--audio` path depends on
        // — is missing. Without this check, the most common invocation
        // (default silent audio) would sail past `check_ffmpeg_capabilities`
        // and only fail once `run_encode` is already streaming frames,
        // which is exactly the mid-encode failure this module's startup
        // probe exists to prevent.
        let filters_without_anullsrc = " ... abuffer           |->A       Buffer audio frames, and make them accessible to the filterchain.\n";
        let missing = missing_capabilities(
            SAMPLE_ENCODERS_FULL,
            SAMPLE_DECODERS_FULL,
            SAMPLE_MUXERS_FULL,
            filters_without_anullsrc,
        );
        assert_eq!(
            missing,
            vec!["filter:anullsrc (lavfi, needed for the default silent audio track)"]
        );
    }

    // ---- clamp_crop_y ----

    #[test]
    fn clamp_crop_y_passes_through_a_position_that_fits() {
        assert_eq!(clamp_crop_y(500.0, 5000, 1080), 500);
    }

    #[test]
    fn clamp_crop_y_pulls_back_a_window_that_would_run_past_the_bottom() {
        // camera_y_at's own contract allows it to return up to image_height
        // (see pacing.rs) — a 1080-tall window whose top sits at
        // image_height would read 1080px past the actual bottom of the
        // image without this clamp.
        assert_eq!(clamp_crop_y(5000.0, 5000, 1080), 5000 - 1080);
    }

    #[test]
    fn clamp_crop_y_never_goes_negative() {
        assert_eq!(clamp_crop_y(-10.0, 5000, 1080), 0);
    }

    // ---- extract_frame ----

    fn synthetic_image(width: u32, height: u32) -> Vec<u8> {
        // Each row's first byte is the row index (mod 256), so a slice's
        // identity is verifiable without needing a real image at all.
        let row_bytes = width as usize * 3;
        let mut buf = vec![0u8; row_bytes * height as usize];
        for row in 0..height as usize {
            buf[row * row_bytes] = (row % 256) as u8;
        }
        buf
    }

    #[test]
    fn extract_frame_returns_exactly_the_requested_rows() {
        let img = synthetic_image(4, 100);
        let frame = extract_frame(&img, 4, 4, 10, 20).unwrap();
        assert_eq!(frame.len(), 4 * 3 * 10);
        // First byte of the slice must be row 20's marker, not row 0's or
        // row 19's — this is the mutation that would silently shift every
        // frame by one row and never show up in a length-only assertion.
        assert_eq!(frame[0], 20);
        let last_row_start = (10 - 1) * 4 * 3;
        assert_eq!(frame[last_row_start], 29);
    }

    #[test]
    fn extract_frame_rejects_a_width_mismatch_instead_of_silently_scaling() {
        let img = synthetic_image(2000, 100);
        let result = extract_frame(&img, 2000, 1920, 10, 0);
        assert!(result.is_err());
    }

    #[test]
    fn extract_frame_reports_a_short_buffer_instead_of_panicking() {
        let img = synthetic_image(4, 5); // far shorter than one 10-row frame
        let result = extract_frame(&img, 4, 4, 10, 0);
        assert!(result.is_err());
    }

    // ---- parse_ffmpeg_duration ----

    #[test]
    fn parses_a_realistic_ffmpeg_duration_line() {
        let stderr = "Input #0, wav, from 'x.wav':\n  Duration: 00:03:15.20, bitrate: 320 kb/s\n";
        let secs = parse_ffmpeg_duration(stderr).unwrap();
        assert!((secs - (3.0 * 60.0 + 15.20)).abs() < 1e-6);
    }

    #[test]
    fn missing_duration_line_returns_none_not_a_panic() {
        assert_eq!(parse_ffmpeg_duration("no duration info here"), None);
    }

    // ---- audio_delta_message ----

    #[test]
    fn matching_lengths_produce_no_message() {
        assert_eq!(audio_delta_message(240.0, 240.01), None);
    }

    #[test]
    fn longer_audio_is_reported_as_longer_with_the_correct_delta() {
        let msg = audio_delta_message(240.0, 244.5).unwrap();
        assert!(msg.contains("longer"), "{msg}");
        assert!(msg.contains("4.50"), "{msg}");
    }

    #[test]
    fn shorter_audio_is_reported_as_shorter_with_the_correct_delta() {
        let msg = audio_delta_message(240.0, 235.0).unwrap();
        assert!(msg.contains("shorter"), "{msg}");
        assert!(msg.contains("5.00"), "{msg}");
    }

    // ---- build_encode_args ----

    fn args_contains_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn built_args_always_force_yuv420p_and_never_yuv444p() {
        // The one assertion this whole module exists to protect, per the
        // task's explicit warning: a future "optimisation" swapping in
        // yuv444p would break iOS/QuickTime playback silently.
        let config = EncodeConfig::default();
        let args = build_encode_args(&config, Path::new("out/x.mp4"), 10.0);
        assert!(args_contains_pair(&args, "-vf", "format=yuv420p"));
        assert!(!args.iter().any(|a| a.contains("yuv444p")));
    }

    #[test]
    fn built_args_request_h264_in_an_mp4_with_faststart() {
        let config = EncodeConfig::default();
        let args = build_encode_args(&config, Path::new("out/x.mp4"), 10.0);
        assert!(args_contains_pair(&args, "-c:v", "libx264"));
        assert!(args_contains_pair(&args, "-movflags", "+faststart"));
        assert!(args.last().unwrap().ends_with("x.mp4"));
    }

    #[test]
    fn built_args_pin_single_threaded_determinism() {
        let config = EncodeConfig::default();
        let args = build_encode_args(&config, Path::new("out/x.mp4"), 10.0);
        assert!(args_contains_pair(&args, "-threads", "1"));
        assert!(args.iter().any(|a| a == "+bitexact"));
    }

    #[test]
    fn silent_audio_is_bounded_to_the_video_duration_but_a_supplied_file_is_not() {
        // Mutation this catches: applying -t to a supplied --audio file
        // (which the task explicitly forbids — it must be muxed at its own
        // natural length) or, the mirror bug, leaving the lavfi silent
        // source unbounded (which would make ffmpeg encode forever, since
        // anullsrc is an infinite generator).
        let silent_config = EncodeConfig {
            audio: AudioSource::Silent,
            ..EncodeConfig::default()
        };
        let silent_args = build_encode_args(&silent_config, Path::new("out/x.mp4"), 42.0);
        assert!(args_contains_pair(&silent_args, "-t", "42"));

        let file_config = EncodeConfig {
            audio: AudioSource::File(PathBuf::from("voice.wav")),
            ..EncodeConfig::default()
        };
        let file_args = build_encode_args(&file_config, Path::new("out/x.mp4"), 42.0);
        assert!(!file_args.iter().any(|a| a == "-t"));
        assert!(file_args.iter().any(|a| a == "voice.wav"));
    }

    #[test]
    fn crf_and_preset_are_threaded_through_verbatim() {
        let config = EncodeConfig {
            crf: 23,
            preset: "veryslow".to_string(),
            ..EncodeConfig::default()
        };
        let args = build_encode_args(&config, Path::new("out/x.mp4"), 10.0);
        assert!(args_contains_pair(&args, "-crf", "23"));
        assert!(args_contains_pair(&args, "-preset", "veryslow"));
    }

    // ---- FrameIter ----

    #[test]
    fn frame_iter_yields_exactly_frame_count_frames_each_of_the_right_size() {
        // image_width must be VIDEO_WIDTH: extract_frame (called by
        // FrameIter::next) hard-rejects any other width, per this module's
        // "the source PNG's width must already match the camera" rule.
        let image_height = 10_000;
        let img = synthetic_image(VIDEO_WIDTH, image_height);
        let segments = vec![Segment {
            y_start: 0.0,
            y_end: 8000.0,
            duration_secs: 2.0, // 2s * FPS(30) = 60 frames
        }];
        let dwell_pivot_text: Vec<Option<(String, String)>> = vec![None];
        let iter = FrameIter {
            raw_rgb: &img,
            segments: &segments,
            dwell_pivot_text: &dwell_pivot_text,
            image_width: VIDEO_WIDTH,
            image_height,
            frame_count: 60,
            next: 0,
        };
        let frames: Vec<Cow<'_, [u8]>> = iter.collect();
        assert_eq!(frames.len(), 60);
        assert!(frames
            .iter()
            .all(|f| f.len() == VIDEO_WIDTH as usize * 3 * VIDEO_HEIGHT as usize));
    }

    #[test]
    fn frame_iter_produces_distinct_advancing_frames_for_a_moving_segment() {
        // Finding C (the review's confirmed vacuous test): a mutation of
        // `let t = self.next as f64 / FPS as f64;` down to a hardcoded
        // `let t = 0.0;` inside `FrameIter::next` left
        // `frame_iter_yields_exactly_frame_count_frames_each_of_the_right_size`
        // fully green, because that test only ever asserts frame COUNT and
        // SIZE — never that two frames actually differ, or that the crop
        // advances. This test asserts on content instead: frame 0 and a
        // later frame within the same moving (non-dwell) segment must be
        // byte-distinct, and the later frame's crop must sit strictly
        // further down the source image than frame 0's.
        //
        // Confirmed red under that exact mutation (applied locally, run,
        // reverted — not committed): with `t` pinned to 0.0 every frame
        // computes the same `y`, so `frames[0] == frames[30]` and
        // `y30 > y0` both fail.
        let image_height = 10_000;
        let img = synthetic_image(VIDEO_WIDTH, image_height);
        let segments = vec![Segment {
            y_start: 0.0,
            y_end: 8000.0,
            duration_secs: 2.0, // 2s * FPS(30) = 60 frames, nonzero velocity throughout
        }];
        let dwell_pivot_text: Vec<Option<(String, String)>> = vec![None];
        let iter = FrameIter {
            raw_rgb: &img,
            segments: &segments,
            dwell_pivot_text: &dwell_pivot_text,
            image_width: VIDEO_WIDTH,
            image_height,
            frame_count: 60,
            next: 0,
        };
        let frames: Vec<Cow<'_, [u8]>> = iter.collect();
        assert_eq!(frames.len(), 60);

        // synthetic_image stamps each row's first byte with that row's index
        // (mod 256) — see synthetic_image's doc comment above — so a moved
        // crop necessarily changes byte 0 of the frame.
        assert_ne!(
            frames[0].as_ref(),
            frames[30].as_ref(),
            "frame 30 must differ from frame 0 across a segment with nonzero velocity"
        );
        let row_marker_at_0 = frames[0][0];
        let row_marker_at_30 = frames[30][0];
        assert!(
            row_marker_at_30 > row_marker_at_0,
            "expected the crop to advance strictly forward: frame 0 row-marker={row_marker_at_0}, \
             frame 30 row-marker={row_marker_at_30}"
        );
    }

    #[test]
    fn frame_iter_composites_a_card_only_on_the_dwell_frame_a_scroll_frame_is_untouched() {
        // End-to-end wiring check for finding A: a segment matched to a
        // `dwell_pivot_text` entry must come back different from the raw
        // crop (the card was drawn), while a segment with no matching entry
        // must come back byte-identical to `extract_frame`'s own output —
        // proving `FrameIter` only composites where it is told to, not on
        // every frame.
        let image_height = 5_000;
        let img = synthetic_image(VIDEO_WIDTH, image_height);
        let segments = vec![
            Segment {
                y_start: 0.0,
                y_end: 0.0,
                duration_secs: 1.0, // dwell: 30 frames at FPS
            },
            Segment {
                y_start: 0.0,
                y_end: 100.0,
                duration_secs: 1.0, // scroll: 30 frames at FPS
            },
        ];
        let dwell_pivot_text: Vec<Option<(String, String)>> = vec![
            Some(("Tag: V1.0.0".to_string(), "Release".to_string())),
            None,
        ];
        let iter = FrameIter {
            raw_rgb: &img,
            segments: &segments,
            dwell_pivot_text: &dwell_pivot_text,
            image_width: VIDEO_WIDTH,
            image_height,
            frame_count: 60,
            next: 0,
        };
        let frames: Vec<Cow<'_, [u8]>> = iter.collect();

        // Frame 0 is inside the dwell segment: its bytes must differ from
        // the raw (uncomposited) crop at the same y, somewhere inside the
        // card's own region.
        let raw_dwell_crop =
            extract_frame(&img, VIDEO_WIDTH, VIDEO_WIDTH, VIDEO_HEIGHT, 0).unwrap();
        assert_ne!(
            frames[0].as_ref(),
            raw_dwell_crop,
            "the dwell frame must have a card composited onto it"
        );

        // Frame 45 (15 frames into the second, scroll segment) has no
        // matching dwell_pivot_text entry: it must come back exactly as
        // extract_frame produced it — a zero-copy borrow, no card drawn.
        assert!(
            matches!(frames[45], Cow::Borrowed(_)),
            "a scroll frame with no dwell text must stay a zero-copy borrow"
        );
    }

    // ---- pivot callout card drawing ----

    #[test]
    fn card_pixels_are_confined_to_the_card_region_not_the_frame() {
        // The review's own suggested assertion: "a frame-with-card differs
        // from frame-without in exactly the card region." Starting from an
        // all-zero frame makes both halves of that claim checkable: any
        // byte outside [CARD_X, CARD_X+CARD_WIDTH) x [CARD_Y,
        // CARD_Y+CARD_HEIGHT) must stay exactly 0, and at least one byte
        // inside that region must have changed.
        let mut frame = vec![0u8; VIDEO_WIDTH as usize * VIDEO_HEIGHT as usize * 3];
        draw_pivot_card(
            &mut frame,
            "Tag: v1.0.0",
            "Release v1.0.0 - Ada, Aug 5, 2026",
        );

        let mut changed_inside_card = false;
        for y in 0..VIDEO_HEIGHT {
            for x in 0..VIDEO_WIDTH {
                let idx = (y as usize * VIDEO_WIDTH as usize + x as usize) * 3;
                let px = &frame[idx..idx + 3];
                let inside_card = (CARD_X..CARD_X + CARD_WIDTH).contains(&x)
                    && (CARD_Y..CARD_Y + CARD_HEIGHT).contains(&y);
                if inside_card {
                    if px != [0, 0, 0] {
                        changed_inside_card = true;
                    }
                } else {
                    assert_eq!(
                        px,
                        [0, 0, 0],
                        "pixel ({x}, {y}) outside the card's own rectangle was touched"
                    );
                }
            }
        }
        assert!(
            changed_inside_card,
            "drawing a card onto an all-zero frame produced no visible change at all"
        );
    }

    #[test]
    fn card_drawing_never_panics_on_a_label_wider_than_the_card() {
        // draw_text_line_clamped documents "silently cut off, never
        // overflow" for text that runs past the card's own right edge —
        // this exercises that path directly with a deliberately oversized
        // label, rather than trusting the cap on chapters.rs's own output
        // (a tag/branch name is not word-capped the way a card's detail
        // line is).
        let mut frame = vec![0u8; VIDEO_WIDTH as usize * VIDEO_HEIGHT as usize * 3];
        let long_label = "X".repeat(500);
        draw_pivot_card(&mut frame, &long_label, &long_label);
        // Reaching this line without a panic and without corrupting memory
        // outside the frame buffer (which `cargo test` would catch as UB/
        // a segfault, not a clean assertion failure) is the assertion.
    }

    #[test]
    fn glyph_rows_falls_back_to_blank_for_an_unsupported_character() {
        // This crate's font is deliberately uppercase-plus-punctuation only
        // (see the "Pivot callout cards" module doc comment) — an accented
        // letter or other unsupported character must render as blank space,
        // not panic.
        assert_eq!(glyph_rows('É'), [0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(glyph_rows('@'), [0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn prepare_card_text_upper_cases_and_substitutes_the_two_unicode_marks() {
        let out = prepare_card_text("merge: fix bug — ada…");
        assert_eq!(out, "MERGE: FIX BUG - ADA...");
    }

    // ---- finding B: the timeline must not extend past where the camera
    // ---- can physically scroll ----

    #[test]
    fn last_frame_crop_y_reaches_exactly_the_scrollable_bottom_when_the_caller_passes_the_reduced_height(
    ) {
        // Finding B (major, measured): `camera_y_at` spans [0, image_height]
        // (whatever `image_height` its caller built the timeline with), but
        // `clamp_crop_y` (this module) pins the crop window at
        // `image_height - viewport_h`. If a caller (main.rs, pre-fix) builds
        // the timeline against the FULL source image height, the last
        // several seconds of video hold a frozen frame: `y` keeps climbing
        // past `image_height - viewport_h` while `clamp_crop_y` keeps
        // returning the same pinned crop.
        //
        // The fix is a caller-side argument change outside this lane's file
        // set (main.rs, not encode.rs — see this crate's repair-pass task):
        // pass `image_height - viewport_h` (floored at 0), not the full
        // image height, as the "image_height" argument to
        // `pacing::commit_density`/`pacing::build_timeline`. This test
        // proves the mechanism that fix depends on: given a timeline built
        // against that reduced height, the LAST frame's crop y lands exactly
        // on `image_height - viewport_h` — no held tail beyond the one
        // frame that legitimately belongs there.
        use crate::pacing::{build_timeline, commit_density, speed_multipliers};

        // `scrollable_height` is chosen as an exact multiple of
        // `band_height` deliberately: `build_timeline` gives every band an
        // equal time-share purely by its speed multiplier (pacing.rs:141-
        // 144, `band_weight = 1.0 / mult`), regardless of that band's own
        // pixel span — so a *truncated* last band (image_height not a clean
        // multiple of band_height) legitimately crawls far slower per-pixel
        // than the others, which is a real, separate property of
        // build_timeline's own time-allocation model, not the frozen-tail
        // bug this test exists to isolate. An exact multiple, combined with
        // zero commits (uniform density -> uniform multiplier -> uniform
        // velocity throughout), keeps this test's only variable the one
        // finding B is actually about: whether the timeline's own y-range
        // matches where the camera can physically stop.
        let full_image_height = 4_080.0_f64;
        let viewport_h = VIDEO_HEIGHT as f64;
        let scrollable_height = (full_image_height - viewport_h).max(0.0); // 3000.0

        let band_height = 300.0; // 3000.0 / 300.0 == 10 exactly
        let commits = [];
        // This is the fixed call shape: `scrollable_height`, not
        // `full_image_height`, is what main.rs must pass here.
        let density = commit_density(&commits, scrollable_height, band_height);
        let multipliers = speed_multipliers(&density);
        let segments = build_timeline(scrollable_height, band_height, &multipliers, &[], 60.0, 3.0);

        let total = total_duration(&segments);
        let y_at_end = camera_y_at(&segments, total);
        assert!(
            (y_at_end - scrollable_height).abs() < 1e-6,
            "the timeline itself must end exactly at the scrollable height, got {y_at_end}"
        );

        // clamp_crop_y, given the REAL full image height, must place the
        // last frame's crop top exactly where the camera physically stops —
        // not short of it, not past it.
        let crop_y_at_end = clamp_crop_y(y_at_end, full_image_height as u32, VIDEO_HEIGHT);
        assert_eq!(crop_y_at_end, full_image_height as u32 - VIDEO_HEIGHT);

        // And the frame one tick before the end must NOT already be pinned
        // to that same crop position — proving there is no multi-frame
        // plateau at the very end, only the single legitimate final frame.
        let one_frame_secs = 1.0 / FPS as f64;
        let y_before_end = camera_y_at(&segments, total - one_frame_secs);
        let crop_before_end = clamp_crop_y(y_before_end, full_image_height as u32, VIDEO_HEIGHT);
        assert!(
            crop_before_end < crop_y_at_end,
            "the frame one tick before the end is already pinned to the final crop position \
             ({crop_before_end} == {crop_y_at_end}) — that is exactly the frozen-tail bug this \
             test exists to catch"
        );
    }
}
