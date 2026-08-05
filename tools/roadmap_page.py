#!/usr/bin/env python3
"""Regenerate Git-Vista's roadmap page from live GitHub data.

Queries milestones (closed/open issue counts) and recently-merged PRs via the
`gh` CLI, and renders a self-contained HTML page — the same design as Tom's
claude.ai roadmap artifact — to design-docs/roadmap.html. Run it whenever you
want a fresh snapshot; every run stamps its own generation timestamp, so the
page is disposable and safe to regenerate as often as you like. The page
itself is untracked scratch (design-docs/ is gitignored); this script is the
tracked, durable artifact.

Usage:
    python3 tools/roadmap_page.py [--open] [--out FILE] [--review M#=N ...]
    python3 tools/roadmap_page.py --selftest

    python3 tools/roadmap_page.py
    python3 tools/roadmap_page.py --open
    python3 tools/roadmap_page.py --review M2=3 --review M3=1

--review M#=N records N issues "in review" for milestone M# — GitHub's
milestone API gives closed/open issue counts but not "how many open PRs are
against this milestone" (that requires walking every open PR's linked
issues, which is expensive and often ambiguous). Rather than guess, the
generated page always shows 0 in review unless you pass this flag
explicitly — you, the session that knows a funnel chain is mid-flight, are
the source of truth for that one number.

Degrades honestly: if `gh` fails (rate limit, no network, not logged in),
the page still renders — with a visible "LIVE DATA UNAVAILABLE" banner
naming the failure — rather than dying, or worse, silently showing stale
numbers as if they were live.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

REPO_SLUG = "tom2025b/git-vista"

# Milestone-title prefix -> short theme string, matching the claude.ai
# artifact Tom built this page's design from. Falls back to the milestone's
# own GitHub title when the prefix isn't mapped (e.g. M8, or anything
# renamed) — never invented.
THEMES: dict[str, str] = {
    "M1": "Foundation — trust the client",
    "M2": "Daily driver: status to push",
    "M3": "Parallel work & recovery",
    "M4": "History editing",
    "M5": "Investigation & forges",
    "M6": "Teaching professional semantics",
    "M7": "Ecosystem & classroom",
    "M9": "Theater, motion, time",
}


def esc(s: str) -> str:
    """Minimal HTML-escape for text interpolated into markup (not JSON)."""
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


# ---------------------------------------------------------------------------
# gh data fetch — every call reports failure back as a warning string rather
# than raising, so one flaky call degrades the page instead of killing it.
# ---------------------------------------------------------------------------

def run_gh(args: list[str]) -> tuple[bool, str]:
    try:
        proc = subprocess.run(
            ["gh", *args], capture_output=True, text=True, timeout=30,
        )
    except FileNotFoundError:
        return False, "gh CLI not found on PATH"
    except subprocess.TimeoutExpired:
        return False, "gh timed out after 30s"
    if proc.returncode != 0:
        reason = (proc.stderr or proc.stdout or "gh exited non-zero").strip()
        # Keep the reported reason to one line — gh's stderr on auth/network
        # failures is often a short multi-line block; the last line is
        # usually the actual message.
        return False, reason.splitlines()[-1] if reason else "gh exited non-zero"
    return True, proc.stdout


def fetch_milestones(warnings: list[str]) -> list[dict]:
    ok, out = run_gh(["api", f"repos/{REPO_SLUG}/milestones?state=all&per_page=20"])
    if not ok:
        warnings.append(f"milestones: {out}")
        return []
    try:
        data = json.loads(out)
    except json.JSONDecodeError as e:
        warnings.append(f"milestones: could not parse gh output ({e})")
        return []
    if not isinstance(data, list):
        warnings.append("milestones: unexpected response shape from gh api")
        return []
    return data


def fetch_cut_counts(milestones: list[dict], warnings: list[str]) -> dict[str, int]:
    """Per milestone: how many of its closed issues were closed as NOT PLANNED.

    GitHub's milestone metadata counts every closed issue identically, which is
    how the page came to show scope removal as progress the night ADR 0049
    closed 18 never-started issues. The `state_reason` field tells them apart:
    `not_planned` is a cut, everything else (including the null reason on
    pre-field closes, e.g. all of M1's) counts as shipped. Pull requests are
    excluded — the milestone metadata counts them, we do not.

    A failed query warns and returns 0 cuts for that milestone, which makes the
    page OVERSTATE progress — so the warning is surfaced in the footer rather
    than swallowed.
    """
    cuts: dict[str, int] = {}
    for m in milestones:
        key = milestone_key(m.get("title", "") or "")
        number = m.get("number")
        if key is None or number is None or int(m.get("closed_issues") or 0) == 0:
            continue
        ok, out = run_gh([
            "api",
            f"repos/{REPO_SLUG}/issues?milestone={number}&state=closed&per_page=100",
        ])
        if not ok:
            warnings.append(f"cut counts for {key}: {out} — cuts shown as 0, progress may be overstated")
            continue
        try:
            issues = json.loads(out)
            cuts[key] = sum(
                1
                for i in issues
                if isinstance(i, dict)
                and "pull_request" not in i
                and i.get("state_reason") == "not_planned"
            )
        except (json.JSONDecodeError, TypeError) as e:
            warnings.append(f"cut counts for {key}: unparseable ({e}) — cuts shown as 0")
    return cuts


def fetch_merged_last_24h(warnings: list[str]) -> int | None:
    ok, out = run_gh([
        "pr", "list", "--state", "merged", "--limit", "30", "--json", "mergedAt",
    ])
    if not ok:
        warnings.append(f"merged PRs: {out}")
        return None
    try:
        prs = json.loads(out)
    except json.JSONDecodeError as e:
        warnings.append(f"merged PRs: could not parse gh output ({e})")
        return None
    cutoff = datetime.now(timezone.utc) - timedelta(hours=24)
    count = 0
    for pr in prs:
        merged_at = pr.get("mergedAt")
        if not merged_at:
            continue
        try:
            dt = datetime.fromisoformat(merged_at.replace("Z", "+00:00"))
        except ValueError:
            continue
        if dt >= cutoff:
            count += 1
    return count


# ---------------------------------------------------------------------------
# shaping: raw gh JSON -> rows the template renders
# ---------------------------------------------------------------------------

def parse_review_overrides(pairs: list[str]) -> dict[str, int]:
    out: dict[str, int] = {}
    for p in pairs:
        if "=" not in p:
            raise SystemExit(f"--review expects M#=N (e.g. M2=3), got {p!r}")
        k, v = p.split("=", 1)
        k = k.strip().upper()
        try:
            out[k] = int(v.strip())
        except ValueError:
            raise SystemExit(f"--review value must be an integer, got {p!r}")
    return out


def milestone_key(title: str) -> str | None:
    m = re.match(r"^(M\d+)\b", title.strip(), re.IGNORECASE)
    return m.group(1).upper() if m else None


def _sort_key(m: dict) -> tuple:
    key = milestone_key(m.get("title", "")) or ""
    num_match = re.match(r"M(\d+)", key)
    num = int(num_match.group(1)) if num_match else 10_000
    return (num, m.get("title", ""))


def build_rows(
    milestones: list[dict],
    review_overrides: dict[str, int],
    cut_counts: dict[str, int] | None = None,
) -> tuple[list[dict], str | None]:
    cut_counts = cut_counts or {}
    rows: list[dict] = []
    for m in sorted(milestones, key=_sort_key):
        title = (m.get("title") or "").strip()
        key = milestone_key(title) or (title[:12] or "?")
        theme = THEMES.get(key, title or key)
        closed_raw = int(m.get("closed_issues") or 0)
        cut = min(cut_counts.get(key, 0), closed_raw)
        # "closed" from here on means SHIPPED. Cuts are scope removal, drawn
        # outside the progress bar and outside every denominator — removing
        # work must never look like finishing it (the night of ADR 0049
        # taught exactly that lesson on this exact page's numbers).
        closed = closed_raw - cut
        open_ = int(m.get("open_issues") or 0)
        review = review_overrides.get(key, 0)
        rows.append({
            "key": key,
            "theme": theme,
            "closed": closed,
            "cut": cut,
            "review": review,
            "open": open_,
            "gh_state": m.get("state", "open"),
        })

    # "Current" = the lowest-numbered milestone that's still open with work
    # left (open issues, or a review override saying work is in flight).
    # Falls back to the lowest-numbered open milestone with no work counted
    # yet, and to None (no tag) if every milestone is closed.
    current_key = None
    for r in rows:
        if r["gh_state"] == "open" and (r["open"] > 0 or r["review"] > 0):
            current_key = r["key"]
            break
    if current_key is None:
        for r in rows:
            if r["gh_state"] == "open":
                current_key = r["key"]
                break

    for r in rows:
        r["current"] = r["key"] == current_key
        r["total"] = r["closed"] + r["review"] + r["open"]
        if r["open"] == 0 and r["review"] == 0:
            # An all-cut milestone shipped NOTHING — "Retired", never
            # "Shipped". (M7 was exactly this shape and the old logic
            # would have labeled it Shipped.)
            if r["closed"] > 0:
                r["state_label"] = "Shipped"
            elif r["cut"] > 0:
                r["state_label"] = "Retired"
            else:
                r["state_label"] = "No issues"
        elif r["closed"] == 0 and r["review"] == 0:
            r["state_label"] = "Planned"
        else:
            r["state_label"] = "In progress"

    return rows, current_key


def compute_tiles(rows: list[dict], current_key: str | None, merged_24h: int | None) -> dict:
    closed_total = sum(r["closed"] for r in rows)
    total_total = sum(r["total"] for r in rows)
    pct = round(100 * closed_total / total_total) if total_total else 0
    current_row = next((r for r in rows if r["key"] == current_key), None)
    return {
        "closed_total": closed_total,
        "total_total": total_total,
        "pct": pct,
        "current_key": current_key or "—",
        "current_closed": current_row["closed"] if current_row else 0,
        "current_open": current_row["open"] if current_row else 0,
        "current_review": current_row["review"] if current_row else 0,
        "merged_24h": merged_24h,
    }


# ---------------------------------------------------------------------------
# rendering — CSS and JS below are copied verbatim from Tom's claude.ai
# roadmap artifact (design reference), plus one small additive .banner rule
# for the "LIVE DATA UNAVAILABLE" case, which the artifact never needed to
# show. The one JS change from verbatim: the "you are here" caption used to
# be a hand-typed sentence in the artifact; here it's built from the current
# milestone's own live counts (`d.here`) so it can never say something this
# run's data doesn't back up.
# ---------------------------------------------------------------------------

HTML_TEMPLATE = r"""<title>Git-Vista Roadmap</title>
<style>
  :root {
    color-scheme: light;
    --surface: #fcfcfb;
    --plane: #f9f9f7;
    --ink: #0b0b0b;
    --ink-2: #52514e;
    --muted: #898781;
    --grid: #e1e0d9;
    --ring: rgba(11, 11, 11, 0.10);
    --closed: #2a78d6;
    --review: #86b6ef;
    --track: #f0efec;
    --tag-bg: #eef3fa;
  }
  @media (prefers-color-scheme: dark) {
    :root:where(:not([data-theme="light"])) {
      color-scheme: dark;
      --surface: #1a1a19;
      --plane: #0d0d0d;
      --ink: #ffffff;
      --ink-2: #c3c2b7;
      --muted: #898781;
      --grid: #2c2c2a;
      --ring: rgba(255, 255, 255, 0.10);
      --closed: #3987e5;
      --review: #184f95;
      --track: #383835;
      --tag-bg: #22303f;
    }
  }
  :root[data-theme="dark"] {
    color-scheme: dark;
    --surface: #1a1a19;
    --plane: #0d0d0d;
    --ink: #ffffff;
    --ink-2: #c3c2b7;
    --muted: #898781;
    --grid: #2c2c2a;
    --ring: rgba(255, 255, 255, 0.10);
    --closed: #3987e5;
    --review: #184f95;
    --track: #383835;
    --tag-bg: #22303f;
  }
  body {
    background: var(--plane);
    color: var(--ink);
    font: 15px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif;
    margin: 0;
    padding: 32px 20px 48px;
  }
  .wrap { max-width: 780px; margin: 0 auto; display: flex; flex-direction: column; gap: 24px; }
  header { display: flex; align-items: baseline; gap: 12px; flex-wrap: wrap; }
  h1 { font-size: 22px; font-weight: 650; margin: 0; text-wrap: balance; }
  .shipped-tag {
    font: 600 12px/1 ui-monospace, "SF Mono", Menlo, monospace;
    color: var(--closed);
    background: var(--tag-bg);
    border: 1px solid var(--ring);
    border-radius: 4px;
    padding: 4px 8px;
  }
  .sub { color: var(--ink-2); margin: 0; font-size: 14px; }

  .tiles { display: grid; grid-template-columns: repeat(auto-fit, minmax(160px, 1fr)); gap: 12px; }
  .tile {
    background: var(--surface);
    border: 1px solid var(--ring);
    border-radius: 8px;
    padding: 14px 16px;
    display: flex; flex-direction: column; gap: 2px;
  }
  .tile .label { font-size: 11px; letter-spacing: 0.06em; text-transform: uppercase; color: var(--muted); }
  .tile .value { font-size: 26px; font-weight: 650; }
  .tile .note { font-size: 12.5px; color: var(--ink-2); }
  .tile .value .of { font-size: 15px; font-weight: 500; color: var(--muted); }

  .panel {
    background: var(--surface);
    border: 1px solid var(--ring);
    border-radius: 8px;
    padding: 18px 18px 14px;
  }
  .panel h2 { font-size: 13px; margin: 0 0 4px; font-weight: 600; }
  .legend { display: flex; gap: 16px; margin: 0 0 14px; font-size: 12.5px; color: var(--ink-2); flex-wrap: wrap; }
  .legend .key { display: inline-flex; align-items: center; gap: 6px; }
  .swatch { width: 10px; height: 10px; border-radius: 2px; display: inline-block; }
  .swatch.closed { background: var(--closed); }
  .swatch.review { background: var(--review); }
  .swatch.track { background: var(--track); box-shadow: inset 0 0 0 1px var(--ring); }

  .rows { display: flex; flex-direction: column; gap: 10px; }
  .row { display: grid; grid-template-columns: 34px 1fr 62px; align-items: center; gap: 12px; }
  .row .name { font: 600 12.5px/1.2 ui-monospace, "SF Mono", Menlo, monospace; color: var(--ink-2); text-align: right; }
  .row .count { font-size: 12.5px; color: var(--muted); font-variant-numeric: tabular-nums; }
  .row.current .name { color: var(--ink); }
  .bar { display: flex; height: 20px; align-items: stretch; }
  .seg {
    min-width: 0;
    margin-right: 2px;
    cursor: default;
    outline-offset: 1px;
  }
  .seg:last-child { margin-right: 0; border-radius: 0 4px 4px 0; }
  .seg:focus-visible { outline: 2px solid var(--ink); }
  .seg.closed { background: var(--closed); }
  .seg.cut {
    background: repeating-linear-gradient(45deg, var(--muted) 0 4px, var(--track) 4px 8px);
    margin-left: 3px; border-radius: 4px; opacity: 0.85;
  }
  .seg.review { background: var(--review); }
  .seg.track { background: var(--track); box-shadow: inset 0 0 0 1px var(--ring); }
  .seg:hover { filter: brightness(1.08); }
  .here {
    grid-column: 2;
    font-size: 11px; letter-spacing: 0.05em; text-transform: uppercase;
    color: var(--ink-2); padding-top: 2px;
  }
  .here::before { content: "\2191 "; color: var(--closed); }

  .tooltip {
    position: fixed;
    pointer-events: none;
    background: var(--ink);
    color: var(--surface);
    font-size: 12.5px;
    padding: 6px 9px;
    border-radius: 5px;
    max-width: 260px;
    z-index: 10;
    display: none;
  }
  .tooltip b { font-variant-numeric: tabular-nums; }

  table { border-collapse: collapse; width: 100%; font-size: 13px; }
  th, td { text-align: left; padding: 6px 10px; border-bottom: 1px solid var(--grid); }
  th { font-size: 11px; letter-spacing: 0.06em; text-transform: uppercase; color: var(--muted); font-weight: 600; }
  td.num, th.num { text-align: right; font-variant-numeric: tabular-nums; }
  td.mono { font-family: ui-monospace, "SF Mono", Menlo, monospace; font-size: 12.5px; }
  .foot { font-size: 12.5px; color: var(--muted); margin: 0; }
  @media (prefers-reduced-motion: no-preference) {
    .seg { transition: filter 120ms ease; }
  }

  .banner {
    background: #fdf3d8;
    color: #6b4a00;
    border: 1px solid #e8c66a;
    border-radius: 8px;
    padding: 12px 16px;
    font-size: 13.5px;
    font-weight: 600;
  }
  @media (prefers-color-scheme: dark) {
    :root:where(:not([data-theme="light"])) .banner {
      background: #3a2e05;
      color: #f5d477;
      border-color: #6b5416;
    }
  }
  :root[data-theme="dark"] .banner {
    background: #3a2e05;
    color: #f5d477;
    border-color: #6b5416;
  }
</style>

<div class="wrap">
  __BANNER_HTML__
  <header>
    <h1>Git-Vista &mdash; release roadmap</h1>
    <span class="shipped-tag">__SHIPPED_TAG__</span>
  </header>
  <p class="sub">__SUBTITLE__</p>

  <div class="tiles">
    <div class="tile">
      <span class="label">Issues closed</span>
      <span class="value">__TILE1_VALUE__</span>
      <span class="note">__TILE1_NOTE__</span>
    </div>
    <div class="tile">
      <span class="label">Current milestone</span>
      <span class="value">__TILE2_VALUE__</span>
      <span class="note">__TILE2_NOTE__</span>
    </div>
    <div class="tile">
      <span class="label">Merged, last 24h</span>
      <span class="value">__TILE3_VALUE__</span>
      <span class="note">__TILE3_NOTE__</span>
    </div>
  </div>

  <div class="panel">
    <h2>Issues per milestone</h2>
    <div class="legend" aria-hidden="true">
      <span class="key"><span class="swatch closed"></span>Closed</span>
      <span class="key"><span class="swatch review"></span>In review</span>
      <span class="key"><span class="swatch track"></span>Open</span>
    </div>
    <div class="rows" id="rows"></div>
  </div>

  <div class="panel">
    <h2>Table view</h2>
    <table>
      <thead>
        <tr><th>Milestone</th><th>Theme</th><th class="num">Shipped</th><th class="num">In review</th><th class="num">Open</th><th class="num">Cut</th><th>State</th></tr>
      </thead>
      <tbody id="tbody"></tbody>
    </table>
  </div>

  <p class="foot">__FOOTER__</p>
</div>

<div class="tooltip" id="tip"></div>

<script>
  const DATA = __DATA_JSON__;
  const MAX = Math.max(1, ...DATA.map(d => d.closed + d.review + d.open + d.cut));
  const rows = document.getElementById("rows");
  const tbody = document.getElementById("tbody");
  const tip = document.getElementById("tip");

  const SEGDEFS = [
    ["closed", "Shipped"],
    ["review", "In review"],
    ["track", "Open"],
    ["cut", "Cut (not planned)"],
  ];

  for (const d of DATA) {
    const total = d.closed + d.review + d.open;
    const row = document.createElement("div");
    row.className = "row" + (d.current ? " current" : "");

    const name = document.createElement("span");
    name.className = "name";
    name.textContent = d.v;
    row.appendChild(name);

    const bar = document.createElement("div");
    bar.className = "bar";
    bar.setAttribute("role", "img");
    bar.setAttribute("aria-label",
      `${d.v} ${d.theme}: ${d.closed} shipped, ${d.review} in review, ${d.open} open` +
      (d.cut ? `, ${d.cut} cut as not planned` : ""));
    const counts = { closed: d.closed, review: d.review, track: d.open, cut: d.cut };
    for (const [cls, label] of SEGDEFS) {
      const n = counts[cls];
      if (n === 0) continue;
      const seg = document.createElement("div");
      seg.className = "seg " + cls;
      seg.style.width = (n / MAX) * 100 + "%";
      seg.tabIndex = 0;
      const text = cls === "cut"
        ? `${d.v} — ${d.theme}\n${label}: ${n} — scope removed by ADR 0049, not progress`
        : `${d.v} — ${d.theme}\n${label}: ${n} of ${total} issues`;
      seg.addEventListener("mousemove", e => showTip(text, e.clientX, e.clientY));
      seg.addEventListener("mouseleave", hideTip);
      seg.addEventListener("focus", () => {
        const r = seg.getBoundingClientRect();
        showTip(text, r.left + r.width / 2, r.top);
      });
      seg.addEventListener("blur", hideTip);
      bar.appendChild(seg);
    }
    row.appendChild(bar);

    const count = document.createElement("span");
    count.className = "count";
    count.textContent = `${d.closed + d.review}/${total}` + (d.cut ? ` · ${d.cut} cut` : "");
    row.appendChild(count);

    if (d.current) {
      const here = document.createElement("span");
      here.className = "here";
      here.textContent = "you are here — " + d.here;
      row.appendChild(here);
    }
    rows.appendChild(row);

    const tr = document.createElement("tr");
    for (const [content, cls] of [
      [d.v, "mono"], [d.theme, ""], [String(d.closed), "num"],
      [String(d.review), "num"], [String(d.open), "num"],
      [String(d.cut), "num"], [d.state, ""],
    ]) {
      const td = document.createElement("td");
      if (cls) td.className = cls;
      td.textContent = content;
      tr.appendChild(td);
    }
    tbody.appendChild(tr);
  }

  function showTip(text, x, y) {
    tip.textContent = "";
    const lines = text.split("\n");
    lines.forEach((l, i) => {
      if (i) tip.appendChild(document.createElement("br"));
      const node = i === 1 ? document.createElement("b") : document.createElement("span");
      node.textContent = l;
      tip.appendChild(node);
    });
    tip.style.display = "block";
    const pad = 12;
    const w = tip.offsetWidth, h = tip.offsetHeight;
    let left = x + pad, top = y - h - pad;
    if (left + w > innerWidth - 8) left = x - w - pad;
    if (top < 8) top = y + pad;
    tip.style.left = left + "px";
    tip.style.top = top + "px";
  }
  function hideTip() { tip.style.display = "none"; }
</script>
"""


def render_html(
    rows: list[dict],
    tiles: dict,
    current_key: str | None,
    warnings: list[str],
    review_overrides: dict[str, int],
    timestamp: str,
) -> str:
    banner_html = ""
    if warnings:
        reason = "; ".join(warnings)
        banner_html = f'<div class="banner">LIVE DATA UNAVAILABLE — gh failed: {esc(reason)}</div>'

    n = len(rows)
    shipped_count = sum(1 for r in rows if r["state_label"] == "Shipped")
    if current_key:
        shipped_tag = f"{shipped_count} shipped · {esc(current_key)} in progress"
    elif rows:
        shipped_tag = f"{shipped_count} shipped · all caught up"
    else:
        shipped_tag = "no milestone data"

    subtitle = (
        f"{n} milestone{'s' if n != 1 else ''} tracked on GitHub. "
        f"{tiles['closed_total']} of {tiles['total_total']} issues closed "
        f"({tiles['pct']}%). Generated {timestamp}."
    )

    tile1_value = f'{tiles["closed_total"]} <span class="of">/ {tiles["total_total"]}</span>'
    tile1_note = f'{tiles["pct"]}% of the planned roadmap'

    tile2_value = esc(str(tiles["current_key"]))
    if current_key:
        parts = [f'{tiles["current_closed"]} closed', f'{tiles["current_open"]} open']
        if tiles["current_review"]:
            parts.append(f'{tiles["current_review"]} in review')
        tile2_note = " · ".join(parts)
    else:
        tile2_note = "no open milestone"

    if tiles["merged_24h"] is None:
        tile3_value = "—"
        tile3_note = "gh unavailable — see banner"
    else:
        tile3_value = str(tiles["merged_24h"])
        tile3_note = "PRs merged in the last 24 hours"

    footer_bits = ["Counts pulled live from GitHub milestones."]
    if tiles["merged_24h"] is not None:
        footer_bits.append("Merged-PR count from `gh pr list --state merged`.")
    if review_overrides:
        ov = ", ".join(f"{k}={v}" for k, v in sorted(review_overrides.items()))
        footer_bits.append(f"Review counts manually overridden: {ov}.")
    footer_bits.append(f"Page generated {timestamp}.")
    footer = " ".join(footer_bits)

    data_json = json.dumps([
        {
            "v": r["key"],
            "theme": r["theme"],
            "closed": r["closed"],
            "cut": r["cut"],
            "review": r["review"],
            "open": r["open"],
            "state": r["state_label"],
            "current": r["current"],
            "here": (
                (
                    f'{r["closed"]} shipped · {r["open"]} open'
                    + (f' · {r["review"]} in review' if r["review"] else "")
                    + (f' · {r["cut"]} cut' if r["cut"] else "")
                )
                if r["current"]
                else ""
            ),
        }
        for r in rows
    ])
    # Belt-and-braces: a milestone title containing "</script>" must not be
    # able to break out of the script block it's embedded in.
    data_json = data_json.replace("</", "<\\/")

    html = HTML_TEMPLATE
    for token, value in {
        "__BANNER_HTML__": banner_html,
        "__SHIPPED_TAG__": shipped_tag,
        "__SUBTITLE__": esc(subtitle),
        "__TILE1_VALUE__": tile1_value,
        "__TILE1_NOTE__": esc(tile1_note),
        "__TILE2_VALUE__": tile2_value,
        "__TILE2_NOTE__": esc(tile2_note),
        "__TILE3_VALUE__": tile3_value,
        "__TILE3_NOTE__": esc(tile3_note),
        "__FOOTER__": esc(footer),
        "__DATA_JSON__": data_json,
    }.items():
        html = html.replace(token, value)
    return html


# ---------------------------------------------------------------------------
# selftest — fixed fixture, zero subprocess calls, the gate this tool has
# since there's nothing to wire into cargo for a python script.
# ---------------------------------------------------------------------------

FIXTURE_MILESTONES = [
    {"title": "M1 — Foundation", "state": "closed", "closed_issues": 39, "open_issues": 0, "number": 1},
    {"title": "M2 — Daily driver", "state": "open", "closed_issues": 24, "open_issues": 28, "number": 2},
    {"title": "M3 — Parallel work", "state": "open", "closed_issues": 0, "open_issues": 5, "number": 3},
    # M7-shaped: every closed issue is a cut — must read Retired, never Shipped
    {"title": "M7 — Ecosystem", "state": "closed", "closed_issues": 4, "open_issues": 0, "number": 7},
    # M4-shaped: mixed — 2 of its closes are cuts, so shipped is 0 with 4 open
    {"title": "M4 — History", "state": "open", "closed_issues": 2, "open_issues": 4, "number": 4},
]
FIXTURE_CUTS = {"M7": 4, "M4": 2}
FIXTURE_MERGED_24H = 12
FIXTURE_REVIEW = {"M2": 3}
FIXTURE_TIMESTAMP = "2026-08-02 05:16 UTC"


def run_selftest() -> int:
    rows, current_key = build_rows(FIXTURE_MILESTONES, FIXTURE_REVIEW, FIXTURE_CUTS)
    tiles = compute_tiles(rows, current_key, FIXTURE_MERGED_24H)
    html = render_html(
        rows, tiles, current_key,
        warnings=[], review_overrides=FIXTURE_REVIEW, timestamp=FIXTURE_TIMESTAMP,
    )

    ok = True

    def check(label: str, cond: bool) -> None:
        nonlocal ok
        print(f"  [{'ok  ' if cond else 'FAIL'}] {label}")
        if not cond:
            ok = False

    # Expected fixture arithmetic, computed independently of build_rows/
    # compute_tiles so the test isn't just re-asserting its own inputs:
    #   closed_total = 39 + 24 + 0 + 0(M7 all cut) + 0(M4 both cut) = 63
    #   total_total  = 39 + (24+3+28) + 5 + 0(M7) + 4(M4 open)       = 103
    #   pct          = round(100*63/103)                              = 61
    #   (cuts excluded from BOTH numerator and denominator — removing
    #    scope must never move the progress number in either direction)
    expected_closed_total = 63
    expected_total_total = 103
    expected_pct = round(100 * expected_closed_total / expected_total_total)

    check(f"closed_total == {expected_closed_total}", tiles["closed_total"] == expected_closed_total)
    check(f"total_total == {expected_total_total}", tiles["total_total"] == expected_total_total)
    check(f"pct == {expected_pct}", tiles["pct"] == expected_pct)
    check("current milestone == M2", current_key == "M2")

    leftover = re.findall(r"__[A-Z0-9_]+__", html)
    check(f"no unsubstituted {{placeholder}} markers (found {leftover})", not leftover)

    check("timestamp appears at least twice (header sub-line + footer)",
          html.count(FIXTURE_TIMESTAMP) >= 2)

    m = re.search(r"const DATA = (\[.*?\]);", html, re.S)
    check("DATA array present in rendered script", bool(m))
    if m:
        data = json.loads(m.group(1))
        got_keys = {d["v"] for d in data}
        check("DATA has M1, M2, M3, M4, M7", got_keys == {"M1", "M2", "M3", "M4", "M7"})
        m2 = next((d for d in data if d["v"] == "M2"), None)
        check("M2 review override (3) reflected in DATA", m2 is not None and m2["review"] == 3)
        m1 = next((d for d in data if d["v"] == "M1"), None)
        check("M1 state == Shipped", m1 is not None and m1["state"] == "Shipped")
        # The honesty checks ADR 0049 earned: cuts are not progress.
        m7 = next((d for d in data if d["v"] == "M7"), None)
        check("M7 (all cuts) state == Retired, NOT Shipped",
              m7 is not None and m7["state"] == "Retired")
        check("M7 shipped == 0 with cut == 4",
              m7 is not None and m7["closed"] == 0 and m7["cut"] == 4)
        m4 = next((d for d in data if d["v"] == "M4"), None)
        check("M4 shipped == 0 (its 2 closes were cuts), cut == 2",
              m4 is not None and m4["closed"] == 0 and m4["cut"] == 2)

    check(f"grand total ({expected_total_total}) visible in tiles",
          str(expected_total_total) in html)
    check(f"merged-24h count ({FIXTURE_MERGED_24H}) visible in tiles",
          str(FIXTURE_MERGED_24H) in html)

    print("selftest: " + ("PASS" if ok else "FAIL"))
    return 0 if ok else 1


# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--open", action="store_true",
                     help="xdg-open the page after writing it (failure ignored)")
    ap.add_argument("--out", type=Path, default=None,
                     help="output path (default: design-docs/roadmap.html)")
    ap.add_argument("--review", action="append", default=[], metavar="M#=N",
                     help="manual in-review override, e.g. --review M2=3 (repeatable)")
    ap.add_argument("--selftest", action="store_true",
                     help="render against a fixed fixture, no gh calls, exit 0/1")
    args = ap.parse_args()

    if args.selftest:
        return run_selftest()

    review_overrides = parse_review_overrides(args.review)

    warnings: list[str] = []
    milestones = fetch_milestones(warnings)
    cut_counts = fetch_cut_counts(milestones, warnings)
    merged_24h = fetch_merged_last_24h(warnings)

    rows, current_key = build_rows(milestones, review_overrides, cut_counts)
    tiles = compute_tiles(rows, current_key, merged_24h)
    timestamp = datetime.now().astimezone().strftime("%Y-%m-%d %H:%M %Z")

    html = render_html(rows, tiles, current_key, warnings, review_overrides, timestamp)

    out = args.out or (Path(__file__).resolve().parent.parent / "design-docs" / "roadmap.html")
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(html, encoding="utf-8")
    print(str(out.resolve()))

    if warnings:
        print(f"roadmap_page: warning: {'; '.join(warnings)}", file=sys.stderr)

    if args.open:
        try:
            subprocess.run(
                ["xdg-open", str(out)], check=False,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            )
        except FileNotFoundError:
            pass

    return 0


if __name__ == "__main__":
    sys.exit(main())
