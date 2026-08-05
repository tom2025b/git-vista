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

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::pacing::{camera_y_at, total_duration, Segment};

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
/// per-run (`audio`, `out_name`).
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

impl Default for EncodeConfig {
    fn default() -> Self {
        Self {
            crf: 18,
            preset: "medium".to_string(),
            audio: AudioSource::Silent,
            out_name: "scrollcast.mp4".to_string(),
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
        if let Err(e) = stdin.write_all(frame).await {
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

/// Lazily yields each output frame's byte slice in order, computing its
/// source `y` from `camera_y_at` on demand rather than precomputing and
/// storing every frame's slice up front — with 7,200 frames that would be a
/// 7,200-element `Vec<&[u8]>`, cheap in itself, but there is no reason to pay
/// even that when a plain iterator does the same job.
struct FrameIter<'a> {
    raw_rgb: &'a [u8],
    segments: &'a [Segment],
    image_width: u32,
    image_height: u32,
    frame_count: u64,
    next: u64,
}

impl<'a> Iterator for FrameIter<'a> {
    type Item = &'a [u8];

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
        Some(
            extract_frame(
                self.raw_rgb,
                self.image_width,
                VIDEO_WIDTH,
                VIDEO_HEIGHT,
                y_px,
            )
            .expect("frame bounds already validated in encode_video"),
        )
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
    let frames = FrameIter {
        raw_rgb: &raw_rgb,
        segments,
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
        let iter = FrameIter {
            raw_rgb: &img,
            segments: &segments,
            image_width: VIDEO_WIDTH,
            image_height,
            frame_count: 60,
            next: 0,
        };
        let frames: Vec<&[u8]> = iter.collect();
        assert_eq!(frames.len(), 60);
        assert!(frames
            .iter()
            .all(|f| f.len() == VIDEO_WIDTH as usize * 3 * VIDEO_HEIGHT as usize));
    }
}
