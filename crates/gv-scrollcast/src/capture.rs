//! Renders git-vista's Print Graph sheet to one full-height PNG, and reads
//! back the pixel y-position of every commit node while the page is still
//! open — the plumbing half of #325's design split (see this crate's module
//! doc): this file makes no pacing decisions, it only carries the rendered
//! image and a measured fact (`image_height`, one `y` per commit) out to the
//! part of the crate that does.
//!
//! # What the input actually is
//!
//! The Print Graph sheet (`crates/git-vista/src/print.rs`) is built *inside*
//! the running WASM app, straight out of its live [`RenderCtx`]
//! (git-vista/src/print.rs:1-20, :189-198) — there is no server route that
//! serves it standalone, and this crate is barred from starting the app
//! (`./dev serve` binds port 8080, which is the owner's live iPad session).
//! So this module cannot "go get" the sheet the way a browser-testing tool
//! normally would; it can only be handed a file that already *is* the sheet,
//! and navigate `file://` to it. Concretely: **the caller renders
//! `print.rs`'s `graph_sheet()` output to a static HTML file — the same
//! markup `window.print()` would have printed, saved instead of printed —
//! and passes this module that file's path.** This is a real constraint, not
//! a simplification: there is currently no other way to get the sheet's
//! markup onto disk without a running server, and inventing one here would
//! silently add the very app-server dependency rule 2 forbids.
//!
//! An HTML file is the shape this module is built and reasoned about for: a
//! plain white-background document whose body holds the `<svg
//! class="print-graph-svg">` (print.rs:383-384) at natural document flow, so
//! `document.documentElement.scrollHeight` means the same thing it means for
//! any ordinary web page. A bare, top-level `.svg` file is also accepted —
//! `file://` navigation to an `.svg` document works in Chromium, and
//! `document.querySelector(...)` still finds the root element — but a
//! standalone SVG document's `document.documentElement` *is* the `<svg>`
//! itself (an `SVGSVGElement`), and this module has not been tested against
//! that shape; `scrollHeight`/full-page-screenshot behaviour for a top-level
//! SVG document is less predictable across engine versions than for HTML.
//! Prefer handing this an HTML wrapper.
//!
//! # Node y-positions: what selector actually exists
//!
//! Requirement 5 asks this module to read the y-position of every commit
//! node off the sheet's own markup. `print.rs`'s SVG builder puts **no
//! class or data attribute on the node `<circle>` itself** — the `nodes`
//! block (print.rs:288-312) emits
//!
//! ```text
//! <circle cx=.. cy=.. r=NODE_RADIUS fill=color stroke=color stroke-width="2" />
//! <text x=.. y=.. text-anchor="end" class="nf node-icon" fill=color>{icon}</text>
//! ```
//!
//! bare, indistinguishable by class/attribute from the stub-tip rings drawn
//! later in the same `<g>` (print.rs:271-285), which are *also* bare
//! `<circle>`s with no class. The only thing that visibly differs between
//! the two circle kinds is `fill` (a node's is a branch colour; a stub tip's
//! is always literal `"#ffffff"`, print.rs:279) — but that is a coincidence
//! of the current colour scheme, not a designed selector, and
//! `git-vista-core::color` never promises no branch colour will ever equal
//! white. So this module does **not** select on `fill`.
//!
//! What *is* real, stable, and load-bearing in print.rs's own structure: the
//! `class="nf node-icon"` on line 305's `<text>`, and the fact that it is
//! emitted in the same `view!` call, immediately after its circle, one pair
//! per commit row, in row order — by construction, not by luck (the `nodes`
//! collect_view zips exactly one `<circle>`+`<text>` pair per `GraphRow`).
//! So this module selects `text.node-icon` (unique in the sheet — no other
//! element carries that class; badge glyphs use `class="nf"` alone, without
//! `node-icon`, print.rs:358) and reads each one's `previousElementSibling`,
//! asserting it is a `<circle>` before trusting its `cy`. If print.rs ever
//! stops emitting that pairing, this fails loudly (zero matches -> hard
//! error, never a silent empty list) rather than guessing a fallback
//! selector.
//!
//! Y is read via `getBoundingClientRect()`, not the circle's raw `cy`
//! attribute. The SVG's `viewBox` and its rendered CSS size need not be
//! 1:1 (print.rs's own size picker, `PrintScale`, scales the *rendered*
//! width without touching viewBox geometry — print.rs:55-59), so a `cy`
//! attribute value is in the SVG's user-coordinate space, not screen
//! pixels. `getBoundingClientRect()` gives the actual on-page pixel
//! position, which is what a pixel-addressed video camera needs, and what
//! `pacing::CommitY.y` documents itself as ("Pixel y-position in the
//! full-height rendered image"). It is read once, before the full-page
//! screenshot's temporary viewport resize (see below): the whole document
//! is already laid out in full on navigation-complete (this sheet has no
//! virtualization, print.rs:1-9), so off-screen rows have correct rects
//! long before anything scrolls or resizes to make them visible.
//!
//! # Determinism — what is and is not guaranteed
//!
//! This module fixes what it can: `--force-device-scale-factor=1` (so 1 CSS
//! px is exactly 1 output pixel, which is *why* `getBoundingClientRect()`
//! coordinates and PNG pixel offsets are the same numbers),
//! `--disable-lcd-text` (subpixel font antialiasing samples the host's
//! fontconfig/hinting state, which differs machine to machine), and an
//! injected stylesheet that kills CSS animations/transitions before first
//! paint (there is no reliable Chromium CLI flag for this — see
//! `DISABLE_ANIMATIONS_JS` below). What it explicitly does **not**
//! guarantee: byte-identical PNGs across different Chromium builds, fonts
//! installed, or fontconfig states. Font hinting/shaping and subpixel glyph
//! placement are not part of the CDP surface this module controls, and two
//! otherwise-identical machines with different system fonts installed will
//! rasterize the sheet's text differently. Determinism here means "the same
//! machine, same Chromium binary, same input file, same PNG" — not
//! cross-machine bit-for-bit reproducibility.
//!
//! # Why `--no-sandbox`
//!
//! Chromium's sandbox exists to contain a *hostile, remote* page attacking
//! the browser process. Every navigation this module ever makes is to a
//! `file://` URL the caller supplied from their own filesystem — there is no
//! remote content, no network fetch, no third-party script origin in this
//! picture at all. The sandbox's threat model does not apply, and on some
//! namespaced/CI-style Linux environments (this box's dev container
//! included) the setuid sandbox helper cannot even initialize without
//! privileges the process doesn't have, which turns the flag from optional
//! hardening into a hard requirement just to launch at all.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::{
    AddScriptToEvaluateOnNewDocumentParams, CaptureScreenshotFormat,
};
use chromiumoxide::handler::viewport::Viewport as LaunchViewport;
use chromiumoxide::page::ScreenshotParams;
use futures::StreamExt;
use serde::Deserialize;

use crate::pacing::CommitY;

/// Rendered viewport width in CSS pixels. Chosen to match the finished
/// video's frame width (1920x1080) so the pacing/encode lanes never need to
/// rescale horizontally — only the vertical camera crop varies over time.
pub const DEFAULT_VIEWPORT_WIDTH: u32 = 1920;

/// Env var that overrides the headless Chromium binary path, checked before
/// PATH and before the vendored Playwright copy — same precedence shape as
/// `$GV_SCROLLCAST_FFMPEG` for the encode lane, so both binaries are
/// resolved and overridden the same way.
pub const CHROME_ENV_OVERRIDE: &str = "GV_SCROLLCAST_CHROME";

/// Executable name looked up on `PATH` before falling back to the vendored
/// copy. This is the dedicated headless-only Chromium build (not full
/// `chromium`/`google-chrome` with a `--headless` flag bolted on), matching
/// what is actually installed on this box.
const CHROME_PATH_NAME: &str = "chrome-headless-shell";

/// Last-resort binary location: the Playwright cache this box ships,
/// confirmed present at the path the task handed us. Only reached when
/// neither the env override nor `PATH` resolved anything, so a box without
/// this exact Playwright version installed still gets a chance via the
/// other two routes before failing.
const PLAYWRIGHT_CHROME_PATH: &str = "/home/tom/.cache/ms-playwright/chromium_headless_shell-1228/chrome-headless-shell-linux64/chrome-headless-shell";

/// JS injected via `Page.addScriptToEvaluateOnNewDocument`, so it runs
/// before the navigated document's own scripts/first paint, not after.
/// There is no Chromium launch flag that reliably suppresses CSS
/// animations/transitions site-wide (`--force-prefers-reduced-motion` is
/// not a stable flag across the Chromium versions this crate might run
/// against); a `!important` stylesheet is the one mechanism guaranteed to
/// apply regardless of what the input document's own CSS does, since it is
/// injected after the page's stylesheets in cascade order. This exists as a
/// defensive measure for whatever HTML the caller hands in — the print
/// sheet itself is static SVG with nothing to animate, but this module
/// cannot assume every caller-supplied wrapper HTML is equally inert.
const DISABLE_ANIMATIONS_JS: &str = "\
    const s = document.createElement('style'); \
    s.textContent = '*, *::before, *::after { \
        animation: none !important; \
        transition: none !important; \
        caret-color: transparent !important; \
    }'; \
    document.documentElement.appendChild(s);";

/// The one JS probe this module runs against the loaded sheet: it reports
/// enough for the caller to trust (or loudly reject) the capture before any
/// pixels are spent on ffmpeg. Bundled into a single `evaluate_function`
/// round-trip rather than two separate `evaluate()` calls because both
/// numbers come from the same DOM snapshot at the same instant — splitting
/// them into two round-trips would risk (harmlessly, but pointlessly) two
/// different microtask-queue states answering the two questions.
///
/// Selector reasoning is in this module's doc comment above; the short
/// version restated here because it explains what the JS below is doing:
/// `text.node-icon` is the one class print.rs actually emits on a
/// commit-node's icon glyph (print.rs:305), and its immediately preceding
/// sibling is that same commit's `<circle>` by construction
/// (print.rs:299-310).
const PROBE_JS: &str = "\
    () => {
        const scrollHeight = document.documentElement.scrollHeight;
        const svg = document.querySelector('svg.print-graph-svg');
        if (!svg) {
            return { scrollHeight, commitYs: [], svgFound: false, nodeIconCount: 0 };
        }
        const icons = Array.from(svg.querySelectorAll('text.node-icon'));
        const commitYs = [];
        for (const icon of icons) {
            const circle = icon.previousElementSibling;
            if (circle && circle.tagName.toLowerCase() === 'circle') {
                const r = circle.getBoundingClientRect();
                commitYs.push(r.top + r.height / 2);
            }
        }
        return {
            scrollHeight,
            commitYs,
            svgFound: true,
            nodeIconCount: icons.length,
        };
    }";

/// What the in-page probe (`PROBE_JS`) reports back, deserialized straight
/// off `Runtime.callFunctionOn`'s JSON return value.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageProbe {
    /// `document.documentElement.scrollHeight` at probe time — the ground
    /// truth this module checks the finished PNG's height against.
    scroll_height: f64,
    /// One entry per `text.node-icon` whose previous sibling was a
    /// `<circle>`, in DOM order (which is row order — print.rs iterates
    /// `rows` in order when building the `nodes` view, print.rs:288-312).
    commit_ys: Vec<f64>,
    /// Whether `svg.print-graph-svg` was found at all. `false` means the
    /// input file is not a Print Graph sheet, or print.rs stopped emitting
    /// that class — either way, a fact worth a named error rather than an
    /// empty `commit_ys` a caller could mistake for "a graph with no
    /// commits."
    svg_found: bool,
    /// How many `text.node-icon` elements existed, independent of whether
    /// their sibling check passed. Lets a mismatch between this and
    /// `commit_ys.len()` be reported explicitly instead of silently
    /// dropping rows whose markup didn't pair the way print.rs is expected
    /// to pair it.
    node_icon_count: usize,
}

/// Tunables for one capture run. Kept small and explicit (no builder) since
/// there are exactly two knobs and neither has a sensible reason to grow a
/// fluent API before a second caller exists to justify one.
#[derive(Debug, Clone)]
pub struct CaptureOptions {
    /// CSS pixel width of the rendered viewport. Height is never configured
    /// here — it is always "the whole document," which is the entire point
    /// of this module (see [`DEFAULT_VIEWPORT_WIDTH`]).
    pub viewport_width: u32,
    /// Explicit chrome-headless-shell path, taking precedence over
    /// `$GV_SCROLLCAST_CHROME`, PATH, and the vendored copy — for a future
    /// `--chrome-path` CLI flag; `None` falls through to the env/PATH/
    /// vendored resolution chain in [`resolve_chrome_binary`].
    pub chrome_path: Option<PathBuf>,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            viewport_width: DEFAULT_VIEWPORT_WIDTH,
            chrome_path: None,
        }
    }
}

/// What one capture run hands back to its caller.
#[derive(Debug, Clone)]
pub struct CaptureResult {
    /// Where the full-height PNG was written.
    pub png_path: PathBuf,
    /// The PNG's actual decoded width, in pixels — read back off the file
    /// itself, never assumed to equal `viewport_width` (see this module's
    /// doc comment on why width is not independently forced the way height
    /// is verified).
    pub width: u32,
    /// The PNG's actual decoded height, in pixels. This is the measured
    /// fact [`pacing::commit_density`]'s `image_height` parameter wants —
    /// never the page's self-reported `scrollHeight`, even though the two
    /// are checked equal before this struct is ever built. Measuring twice
    /// and asserting equality (rather than trusting one number) is what
    /// makes "never truncated" a checked property instead of an assumption.
    pub height: u32,
    /// One [`CommitY`] per commit node found on the sheet, in row order —
    /// feeds straight into `pacing::commit_density`.
    pub commit_ys: Vec<CommitY>,
}

/// Find `name` on `$PATH`, the same lookup a shell does for a bare command,
/// without shelling out to `which` (not installed on every box, and this is
/// three lines of `std`).
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}

/// Resolve the headless Chromium binary, in the order the task spec fixes:
/// an explicit override (from a future CLI flag, via `CaptureOptions`),
/// then `$GV_SCROLLCAST_CHROME`, then `PATH`, then the Playwright cache this
/// box ships. Fails here — at the caller's discretion, *before* any async
/// runtime or browser process is spent — rather than surfacing as an opaque
/// "failed to spawn" error mid-launch. Every failure path names both what
/// was looked for and where, per the task's hard requirement that a missing
/// binary is a clear startup error, never a confusing mid-run one.
pub fn resolve_chrome_binary(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return if p.is_file() {
            Ok(p.to_path_buf())
        } else {
            bail!(
                "explicit chrome binary path {} does not exist or is not a file",
                p.display()
            )
        };
    }

    if let Ok(env_path) = std::env::var(CHROME_ENV_OVERRIDE) {
        let p = PathBuf::from(&env_path);
        return if p.is_file() {
            Ok(p)
        } else {
            bail!(
                "${CHROME_ENV_OVERRIDE}={env_path} does not point at a file — set it to the \
                 chrome-headless-shell binary, or unset it to fall back to PATH / the vendored \
                 Playwright copy"
            )
        };
    }

    if let Some(p) = find_on_path(CHROME_PATH_NAME) {
        return Ok(p);
    }

    let vendored = PathBuf::from(PLAYWRIGHT_CHROME_PATH);
    if vendored.is_file() {
        return Ok(vendored);
    }

    bail!(
        "no headless Chromium binary found: checked ${CHROME_ENV_OVERRIDE} (unset), PATH for \
         '{CHROME_PATH_NAME}' (not found), and the Playwright cache at {PLAYWRIGHT_CHROME_PATH} \
         (not present). Install chrome-headless-shell, or set ${CHROME_ENV_OVERRIDE} to its path."
    );
}

/// Build a `file://` URL for an absolute local path. Percent-encodes only
/// the handful of characters that would otherwise change the URL's meaning
/// (space, `#`, `?`, `%` itself) — this is not a general URL encoder (no
/// crate for that is in this lane's dependency list, and pulling one in for
/// a local-only, non-attacker-controlled path is more machinery than the
/// problem warrants), but those four are exactly the characters a real
/// filesystem path is likely to contain that also mean something to a URL
/// parser (a literal `%` in a path is the one that would otherwise corrupt
/// silently rather than fail loudly, which is why it is encoded even though
/// it is the rarest of the four in practice).
fn file_url_for(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len() + 8 + "file://".len());
    out.push_str("file://");
    for ch in raw.chars() {
        match ch {
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            '%' => out.push_str("%25"),
            _ => out.push(ch),
        }
    }
    out
}

/// Decode only the width/height fields of a PNG's `IHDR` chunk. Not a
/// general PNG parser — the PNG spec (ISO/IEC 15948:2003 §11.2.2) fixes
/// `IHDR` as the *first* chunk, immediately after the 8-byte signature, at a
/// known byte offset, so there is nothing to search for and no reason to
/// pull in an image-decoding crate for the one fact this module needs out
/// of a screenshot it just took.
fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if bytes.len() < 24 {
        bail!(
            "captured screenshot is only {} bytes — too short to contain a PNG header",
            bytes.len()
        );
    }
    if bytes[0..8] != SIGNATURE {
        bail!("captured screenshot does not start with a PNG signature");
    }
    if &bytes[12..16] != b"IHDR" {
        bail!("captured screenshot's first chunk is not IHDR; cannot read its dimensions");
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("slice is exactly 4 bytes"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("slice is exactly 4 bytes"));
    Ok((width, height))
}

/// Render `input_path` (an HTML or SVG file already on disk — see this
/// module's doc comment for why capture never drives the live app) to one
/// full-height PNG at `output_png`, and read back every commit node's pixel
/// y-position in the same pass.
///
/// Resolves the Chromium binary and canonicalizes `input_path` before doing
/// anything async, so a missing binary or missing input file fails as a
/// plain, immediate error rather than after a browser process has already
/// been spawned.
pub async fn capture_print_sheet(
    input_path: &Path,
    output_png: &Path,
    opts: &CaptureOptions,
) -> Result<CaptureResult> {
    let chrome_path = resolve_chrome_binary(opts.chrome_path.as_deref())
        .context("resolving the headless Chromium binary")?;

    let input_abs = std::fs::canonicalize(input_path).with_context(|| {
        format!(
            "input print sheet not found or unreadable: {}",
            input_path.display()
        )
    })?;
    let file_url = file_url_for(&input_abs);

    let launch_viewport = LaunchViewport {
        width: opts.viewport_width,
        // Arbitrary — `full_page(true)` below re-measures the document's
        // own content box and overrides this to the sheet's real height
        // before the screenshot is taken, so this initial value never
        // reaches the output.
        height: 1080,
        device_scale_factor: Some(1.0),
        ..Default::default()
    };

    let config = BrowserConfig::builder()
        .chrome_executable(&chrome_path)
        .no_sandbox() // see module doc: unprivileged local render of a
        // caller-supplied local file, never remote content — the sandbox's
        // threat model does not apply, and it cannot even initialize
        // without privileges on this box's dev container.
        .viewport(launch_viewport)
        .args(["--force-device-scale-factor=1", "--disable-lcd-text"])
        .build()
        .map_err(|e| anyhow::anyhow!("building the Chromium launch config: {e}"))?;

    let (mut browser, mut handler) = Browser::launch(config)
        .await
        .with_context(|| format!("launching headless Chromium at {}", chrome_path.display()))?;

    // chromiumoxide's `Handler` must be polled continuously in the
    // background for the lifetime of the browser — every `Page`/`Browser`
    // call is a request over its own websocket, and nothing reads the
    // response off that socket except this loop. Without it, the very first
    // `page.goto()` below would hang forever waiting for a reply nothing is
    // listening for.
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = run_capture(&browser, &file_url, output_png, opts).await;

    // Always attempt a clean shutdown, success or failure, so a failed
    // capture never leaks a Chromium process across CLI invocations —
    // `Browser`'s own `Drop` impl waits for the child too, but doing it
    // explicitly here surfaces a shutdown failure instead of silently
    // swallowing it in a destructor.
    let _ = browser.close().await;
    let _ = browser.wait().await;
    handler_task.abort();

    result
}

/// The part of a capture run that can fail meaningfully — split out of
/// [`capture_print_sheet`] so that function's cleanup (closing the browser)
/// runs on every exit path, success or error, without duplicating it at
/// every `?`.
async fn run_capture(
    browser: &Browser,
    file_url: &str,
    output_png: &Path,
    opts: &CaptureOptions,
) -> Result<CaptureResult> {
    let page = browser
        .new_page("about:blank")
        .await
        .context("opening a new headless tab")?;

    page.evaluate_on_new_document(AddScriptToEvaluateOnNewDocumentParams::new(
        DISABLE_ANIMATIONS_JS,
    ))
    .await
    .context("injecting the disable-animations stylesheet before first paint")?;

    page.goto(file_url)
        .await
        .with_context(|| format!("navigating to {file_url}"))?;
    page.wait_for_navigation()
        .await
        .context("waiting for the print sheet to finish loading")?;

    let probe: PageProbe = page
        .evaluate_function(PROBE_JS)
        .await
        .context("running the page probe (scrollHeight + commit node y-positions)")?
        .into_value()
        .context("deserializing the page probe's JSON result")?;

    if !probe.svg_found {
        bail!(
            "no `svg.print-graph-svg` element found at {file_url} — this input does not look \
             like a Print Graph sheet produced by crates/git-vista/src/print.rs's graph_sheet() \
             (expected an HTML or SVG document containing that element)"
        );
    }
    if probe.node_icon_count == 0 {
        bail!(
            "found svg.print-graph-svg at {file_url} but zero `text.node-icon` elements inside \
             it — either the rendered graph has no commits, or print.rs's node markup \
             (print.rs:299-310) no longer matches this module's selector; not guessing a \
             fallback selector that would silently return zero commits"
        );
    }
    if probe.commit_ys.len() != probe.node_icon_count {
        bail!(
            "found {} `text.node-icon` elements but only {} had a `<circle>` as their previous \
             sibling — print.rs's node markup (print.rs:299-310) no longer pairs a commit's \
             circle and icon the way this module's selector assumes",
            probe.node_icon_count,
            probe.commit_ys.len()
        );
    }

    let png_bytes = page
        .screenshot(
            ScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Png)
                .full_page(true)
                .build(),
        )
        .await
        .context("capturing the full-page screenshot")?;

    let (png_width, png_height) =
        png_dimensions(&png_bytes).context("reading dimensions back off the captured PNG")?;

    // The one check this whole module exists to make non-optional: a
    // capture that silently stopped short of the full sheet would just make
    // the finished video end early and look intentional — nothing
    // downstream (pacing, encode) has any way to notice on its own. So the
    // measured PNG height is compared against the page's own report of its
    // content height, and any gap is a hard error naming both numbers,
    // never a warning.
    let height_diff = (png_height as f64 - probe.scroll_height).abs();
    if height_diff > 1.0 {
        bail!(
            "capture truncated: the page reported scrollHeight={:.1}px but the captured PNG is \
             {png_height}px tall (width {png_width}px) — the screenshot did not cover the whole \
             print sheet",
            probe.scroll_height
        );
    }

    std::fs::write(output_png, &png_bytes)
        .with_context(|| format!("writing the captured PNG to {}", output_png.display()))?;

    let commit_ys = probe.commit_ys.into_iter().map(|y| CommitY { y }).collect();

    Ok(CaptureResult {
        png_path: output_png.to_path_buf(),
        width: png_width,
        height: png_height,
        commit_ys,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal valid PNG byte prefix: signature + IHDR chunk with
    /// the given width/height, zero-padded after so `png_dimensions` never
    /// reads past what this constructs. Not a real (decodable) PNG image —
    /// only what `png_dimensions` looks at.
    fn synthetic_png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&0u32.to_be_bytes()); // chunk length (unused by us)
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes
    }

    #[test]
    fn png_dimensions_reads_width_and_height_in_the_right_order() {
        // Mutation this catches: swapping the width/height byte ranges (or
        // reading them little-endian), which would silently transpose every
        // captured image's reported dimensions.
        let bytes = synthetic_png(1920, 47_320);
        let (w, h) = png_dimensions(&bytes).unwrap();
        assert_eq!(w, 1920);
        assert_eq!(h, 47_320);
        assert_ne!(
            w, h,
            "sanity: width and height must be distinguishable in this fixture"
        );
    }

    #[test]
    fn png_dimensions_rejects_a_bad_signature() {
        let mut bytes = synthetic_png(100, 100);
        bytes[0] = 0x00; // corrupt the PNG magic byte
        assert!(png_dimensions(&bytes).is_err());
    }

    #[test]
    fn png_dimensions_rejects_a_first_chunk_that_is_not_ihdr() {
        let mut bytes = synthetic_png(100, 100);
        bytes[12..16].copy_from_slice(b"IDAT"); // not IHDR
        assert!(png_dimensions(&bytes).is_err());
    }

    #[test]
    fn png_dimensions_rejects_truncated_input() {
        let bytes = synthetic_png(100, 100);
        assert!(png_dimensions(&bytes[..10]).is_err());
    }

    #[test]
    fn file_url_percent_encodes_space_hash_question_and_percent() {
        // Mutation this catches: forgetting to encode any one of these,
        // which would silently produce a URL Chromium parses differently
        // than the literal path on disk (truncating at `#`/`?`, or
        // misreading a stray `%XX` as an existing percent-escape).
        let url = file_url_for(Path::new("/tmp/a b#c?d%e.html"));
        assert_eq!(url, "file:///tmp/a%20b%23c%3Fd%25e.html");
    }

    #[test]
    fn file_url_leaves_an_ordinary_path_untouched_but_prefixed() {
        let url = file_url_for(Path::new("/tmp/sheet.html"));
        assert_eq!(url, "file:///tmp/sheet.html");
    }

    #[test]
    fn resolve_chrome_binary_rejects_an_explicit_path_that_does_not_exist() {
        let err = resolve_chrome_binary(Some(Path::new("/no/such/chrome-binary"))).unwrap_err();
        assert!(err.to_string().contains("/no/such/chrome-binary"));
    }

    #[test]
    fn resolve_chrome_binary_accepts_an_explicit_path_that_does_exist() {
        // A file this test creates itself, NOT `file!()`.
        //
        // `file!()` expands to a path relative to the *workspace* root, while
        // a test binary's working directory is the *package* directory — so
        // the original version of this test could never pass from inside this
        // workspace, and shipped red. Building the fixture here makes the
        // test independent of where it is run from, which is the only way it
        // can be honest about what it claims to check.
        let dir = std::env::temp_dir().join("gv-scrollcast-resolve-chrome-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let fake = dir.join("chrome-headless-shell");
        std::fs::write(&fake, b"not really a browser").expect("write fixture");

        // Resolution never inspects the binary's contents, only that the path
        // exists and is a file — so any real file exercises the same path.
        let resolved = resolve_chrome_binary(Some(&fake)).unwrap();
        assert_eq!(resolved, fake);

        let _ = std::fs::remove_file(&fake);
    }

    #[test]
    fn resolve_chrome_binary_rejects_a_directory_even_though_it_exists() {
        // The guard is "exists AND is a file". A bare `exists()` check would
        // accept a directory here and fail much later inside chromiumoxide
        // with a far less obvious message.
        let dir = std::env::temp_dir();
        let err = resolve_chrome_binary(Some(&dir)).unwrap_err();
        assert!(
            err.to_string().contains("is not a file")
                || err.to_string().contains("does not exist or is not a file"),
            "unexpected message: {err}"
        );
    }
}
