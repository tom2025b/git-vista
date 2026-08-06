# gv-scrollcast (#325)

An offline CLI that renders git-vista's **Print Graph** view — one very tall,
light-background SVG sheet of the whole commit graph — to a full-height PNG,
then pans a 1920×1080 camera down it into an MP4 the owner narrates over.

Deliberately outside the served app: it never binds a port, never touches an
axum route, and never starts `./dev serve` or `trunk serve`. It drives a
headless browser against a *file on disk* and shells out to `ffmpeg`.

## What it does

```text
capture   Print Graph sheet (HTML/SVG file) -> one full-height PNG,
          plus every commit node's pixel y-position and rendered metadata
pacing    commit density -> speed curve -> a scroll timeline, built against
          the scrollable height (image height minus one viewport)
chapters  chapters.txt sidecar (a YouTube-style timestamp list) +
          pivot-callout text — merges, ref badges, and month boundaries
          detected off the sheet's own rendered metadata (see "Verified
          end-to-end" below for a real run's chapters.txt and callout frame)
encode    timeline -> cropped frames, with a callout card composited onto
          each dwell frame -> H.264/yuv420p MP4, with +faststart
```

The pacing core (`pacing.rs`) is pure and host-tested — it decides *how* to
scroll. Everything else is plumbing that carries that decision out to a real
browser and a real encoder.

## Prerequisites

No downloads, no network access at runtime. This crate resolves two external
binaries in a fixed order — explicit override, then `$GV_SCROLLCAST_*` env
var, then `PATH`, then a last-resort vendored copy — and fails at **startup**
with a message naming every location checked if neither is found:

| Binary | Env override | Resolution order |
|---|---|---|
| headless Chromium | `$GV_SCROLLCAST_CHROME` | override → env → `PATH` (`chrome-headless-shell`) → vendored Playwright copy |
| ffmpeg | `$GV_SCROLLCAST_FFMPEG` | env → `PATH` (`ffmpeg`) → vendored Playwright copy |

On this box the vendored Playwright fallback for Chromium
(`~/.cache/ms-playwright/chromium_headless_shell-1228/…`) is what
`resolve_chrome_binary` actually finds and uses — there is no
`chrome-headless-shell` on `PATH`. **The Playwright cache's own bundled
`ffmpeg` is deliberately stripped** (built for Playwright's own webm/vp8
screen-recording, not general encoding) — it has no `libx264`, no `aac`, no
PNG decoder, no MP4 muxer, and no `lavfi anullsrc` filter, so it is *not*
what gets used. As of 2026-08-05 this box has a real system **ffmpeg 6.1.1**
at `/usr/bin/ffmpeg` (verified: `libx264` present, MP4 muxer present,
`yuv420p` pixel format produced) — `gv-scrollcast` finds it on `PATH` with no
env var needed. `gv-scrollcast` checks for all five required ffmpeg features
*before* doing any capture or encode work and refuses to start rather than
fail partway through a multi-minute run. If a box only has the stripped
Playwright ffmpeg on `PATH`, point `$GV_SCROLLCAST_FFMPEG` at a full ffmpeg
build instead.

### The input file

`gv-scrollcast` cannot render the Print Graph sheet itself — there is no
server route that serves it standalone, and starting one would mean binding
a port (never done here; port 8080 is the owner's live session). So the
sheet must already be on disk: open git-vista's Print Graph view, use
"Print / Save PDF" → save as HTML instead (or save the page as HTML from the
browser), and pass that file as `<input>`. A bare `.svg` extracted from that
page also works, with caveats noted in `capture.rs`'s doc comment.

## Usage

```text
gv-scrollcast <input> [OPTIONS]

  <input>              the rendered Print Graph sheet (HTML preferred, SVG accepted)
  --duration <secs>     target video length            [default: 240]
  --linear              constant-rate scroll, disables density pacing
  --date-overlay        burn a corner date marker into frames  [not yet implemented — see below]
  --audio <path>        mux this audio file instead of a silent placeholder track
  --width <px>          rendered viewport width          [default: 1920 — see below]
  --max-pivots <n>      cap pivot callouts               [default: 12]
  --out <dir>           output directory                 [default: ./out]
```

### Worked examples

These three are illustrative — none of them was actually run for this doc.
The invocation confirmed against a real capture/encode pass on this box is
in "Verified end-to-end" further down.

```bash
# The 4-minute default: density-paced scroll, silent placeholder audio track,
# written to ./out/scrollcast.mp4
gv-scrollcast sheet.html

# A constant-rate (non-adaptive) pass, 90 seconds, to a scratch directory
gv-scrollcast sheet.html --linear --duration 90 --out /tmp/gv-render

# With a narration track already recorded, muxed at its own natural length
# (never stretched/padded/truncated to fit — a length mismatch is reported,
# not silently corrected)
gv-scrollcast sheet.html --audio narration.wav --out ~/videos/git-vista-tour
```

A finished run reports, and writes:

```text
gv-scrollcast: done
  video:      /home/tom/videos/git-vista-tour/scrollcast.mp4
  chapters:   /home/tom/videos/git-vista-tour/chapters.txt
  capture:    /home/tom/videos/git-vista-tour/capture.png
  7200 frames, 240.0s video
```

`chapters.txt` is a plain `timestamp label` list, one per line, in the exact
format YouTube's video-description chapter parser accepts — paste it
straight into the upload description.

### Verified end-to-end (2026-08-05)

This crate had never actually produced a video before — `ffmpeg` was missing
on this box until now. With a real ffmpeg installed, the CLI was run
end-to-end against a hand-built synthetic sheet (320 commit rows, spanning
several months, with merges and ref badges scattered through) matching
`graph_sheet()`'s exact markup shape (`crates/git-vista/src/print.rs:189-398`
— the `<circle>`+`text.node-icon` pairing, `text.label-msg.pg-msg` /
`text.label-meta.pg-meta`, the same geometry constants from
`crates/git-vista/src/geometry.rs`), since there is still no way to get a
real sheet onto disk without starting the app server (forbidden — see "The
input file" above):

```bash
gv-scrollcast /tmp/gv-scrollcast-sheet.html --duration 20 --out /tmp/gv-scrollcast-out
```

Real output from that run:

```text
gv-scrollcast: chrome  -> /home/tom/.cache/ms-playwright/chromium_headless_shell-1228/chrome-headless-shell-linux64/chrome-headless-shell
gv-scrollcast: ffmpeg  -> /usr/bin/ffmpeg
gv-scrollcast: out dir -> /tmp/gv-scrollcast-out
gv-scrollcast: capturing /tmp/gv-scrollcast-sheet.html
gv-scrollcast: captured 1920x17924 PNG, 320 commit node(s) found
gv-scrollcast: timeline built: 12 segment(s), 36.0s total
gv-scrollcast: encoding (this is the slow part)...
gv-scrollcast: done
video:      /tmp/gv-scrollcast-out/scrollcast.mp4
chapters:   /tmp/gv-scrollcast-out/chapters.txt
capture:    /tmp/gv-scrollcast-out/capture.png
1080 frames, 36.0s video
```

(`--duration 20` came out as 36.0s, not 20.0s. `build_timeline` normally
*carves* dwell time out of `--duration`'s own budget
(`scroll_budget = (target_duration_secs - total_dwell).max(0.0)`,
`pacing.rs:139`) so the finished video matches the target, pivots and all.
But the synthetic sheet's 8 ref-badge rows plus several merge rows gave
`detect_pivots_from_meta` more than `--max-pivots`' default of 12 real
candidates, and 12 pivots × `DEFAULT_DWELL_SECS` (3.0s, `pacing.rs:62`) alone
is 36s — already past the 20s target before any scrolling happens. The
`.max(0.0)` clamp means `scroll_budget` floors at zero rather than going
negative, so the video is exactly `total_dwell` (36.0s) with no scroll time
left over, not a truncated 20s. A short `--duration` on a sheet this dense
in landmarks is a real way to hit this; pass a longer `--duration` or a
smaller `--max-pivots` to keep the two numbers close.)

`ffprobe` on the resulting `scrollcast.mp4` confirms the documented encode
contract, not just its intent:

```text
codec_name=h264, pix_fmt=yuv420p, width=1920, height=1080, fps=30/1
codec_name=aac (audio track)
duration=36.000000
```

`chapters.txt` from that same run — real pivots fired, not just the
mandatory first line:

```text
0:00 Start
0:12 Marked — 0000056
0:24 Marked — 00000d7
```

This sheet's 8 ref-badge rows and several merge rows gave
`detect_pivots_from_meta` well over `--max-pivots`' default of 12
candidates, so (per the dwell-budget math above) the video actually contains
up to 12 real dwell/callout segments, not 2 — the frame extracted below, at
`0:12`, is one of them. `chapters.txt` only shows 2 lines because
`format_chapters` deliberately drops any chapter mark within
`MIN_CHAPTER_GAP_SECS` (10s, `chapters.rs:499`) of the previous surviving
one (`chapters.rs:548`) — a readable YouTube-description chapter list, not
an inventory of every dwell in the video. With the dwells packed back to
back (scroll budget was entirely consumed by dwell time, see above), most of
the 12 collapsed under that 10s filter; only the two spaced far enough apart
survived into the text file.

A frame extracted at `0:12` (`ffmpeg -ss 12.5 -i scrollcast.mp4 -frames:v 1
…`) shows a real callout card composited over the scrolling sheet: a dark
banner reading `MARKED — 0000056` with the detail line `commit #86: do the
thing, take 86 · Grace Hopper · Mar 22 9:00 PM` beneath it — confirming the
encode lane's callout-card path is live, not just chapters.txt text.

### `--out` and this repo's `.gitignore` — why some in-repo paths are refused

`--out` is checked against this repository's own working tree before
anything is written (`resolve_out_dir` in `main.rs`). The rule is not "never
write inside the repo" — that would reject this tool's own documented
default, `./out` — it is **never write into the *tracked* tree**:

- A path **outside** the repository entirely is always accepted.
- A path **inside** the repository is accepted only if git itself already
  ignores it — checked with `git check-ignore`, the same authority `git
  status` uses, not a hand-rolled `.gitignore` parser. The default `./out`
  works out of the box because this repo's own `.gitignore` already carries
  a `/out/` entry for exactly this purpose.
- Any other in-repo path (e.g. `--out ./render-scratch` with no matching
  `.gitignore` entry) is refused with a message naming both the resolved
  path and the repository root, telling you to either pick a directory
  outside the repo or add the path to `.gitignore` yourself.

This check runs *before* the directory is created, so a refused path is
never even `mkdir -p`'d — and it runs before any Chromium/ffmpeg work starts
too, alongside the `--duration`/`--width` checks below.

## Known gaps in this build

Two things worth knowing before assuming a flag does what its name suggests.
Both are explained in full, with file:line citations, in `main.rs`'s top doc
comment; this is the short version.

1. **`--date-overlay` is parsed but not drawn.** Burning a marker into every
   frame needs a hook in the frame-cropping step that doesn't exist yet.
   Passing the flag prints a warning and otherwise changes nothing about the
   output.
2. **`--width` must currently be `1920`.** The encoder's camera is a fixed
   1920×1080; any other *requested* width is rejected at startup (before any
   capture work runs) rather than partway through the pipeline. The
   *captured* width is checked again, separately, right after capture
   returns — see "Fails fast, not partway through" below for why both checks
   exist.

Pivot callouts (merges, ref badges, month boundaries) **do fire** —
`chapters::detect_pivots_from_meta` consumes the capture stage's
sheet-derived `CommitMeta` (summary/author/date-text/has-refs/is-merge) and
feeds real pivots to both the timeline (dwells) and the encoder's callout
cards. This was previously an open gap in this doc; it was closed in an
earlier repair round and confirmed end-to-end in the run below — see
"Verified end-to-end" for what an actual `chapters.txt` and callout frame
look like out of a real run, not just what the code is supposed to do.

## Fails fast, not partway through

Every one of these is checked **before** the slow part of a run (a headless
Chromium capture and an ffmpeg encode can each take minutes), so a bad
argument or a truncated capture is reported immediately rather than deep
inside a multi-minute pass:

| Checked | When | What a failure looks like without it |
|---|---|---|
| `--duration` is finite and positive | At startup, before Chromium is resolved | `NaN`/`inf` silently corrupts the pacing timeline's arithmetic; `0`/negative produces a zero-length timeline that only fails once the encoder reports zero frames |
| `--width` equals the encoder's fixed `1920` | At startup, before Chromium is resolved | The encoder hard-rejects any other width — but only once the first frame is decoded, after the capture already ran |
| The **captured** PNG's actual width also equals `1920` | Immediately after capture returns, before pacing/encode runs | The page's real rendered width can differ from the requested viewport (e.g. content overflowing horizontally); unchecked, this failed the same way as the item above, just after a capture had already spent its minutes of CPU |
| `--out` is creatable and not an un-ignored in-repo path | At startup, before Chromium is resolved | See "`--out` and this repo's `.gitignore`" above |
| The captured PNG's height matches the page's own reported content height | Inside `capture.rs`, right after the screenshot is taken | A silently truncated capture would just make the finished video end early, with nothing downstream able to notice |

## Determinism

Same machine, same pinned Chromium and ffmpeg binaries, same input file →
the same PNG and the same video bytes. Not guaranteed across different
Chromium/ffmpeg builds or different installed fonts — see `capture.rs`'s and
`encode.rs`'s own doc comments for exactly what is and isn't covered and why.
