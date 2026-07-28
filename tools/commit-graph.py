#!/usr/bin/env python3
"""Render a real commit-DAG graph — curved lanes, per-commit nodes — to a
self-contained HTML file. No server, no build step: just `git` and stdlib
Python. Meant as a fallback / alternate view when the live git-vista server
isn't available, or as a reference implementation for porting the same
lane-assignment algorithm into the Rust frontend (graph.rs / lod.rs).

Usage:
    python3 commit-graph.py [--repo PATH] [--since DATE] [--out FILE.html]

    python3 commit-graph.py --repo ~/projects/Git-Vista --since "2026-07-24"
    python3 commit-graph.py --since "7 days ago" --out /tmp/graph.html

--since accepts anything `git log --since` accepts ("2026-07-24 00:00",
"7 days ago", "yesterday", ...). Defaults to 4 days back.

The algorithm (assign_lanes) is the part worth reading if porting this:
it's the same continue-the-lane-of-your-first-parent / open-a-new-lane-
for-a-merge-source approach every git graph tool (gitk, GitKraken, git
log --graph) uses. It runs in one forward pass over commits in the order
`git log` already gives them (newest first, --topo-order so a parent
never appears before all its children), and needs nothing but each
commit's hash and parent hashes.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path


def run_git_log(repo: Path, since: str) -> list[dict]:
    """One process spawn: hash, parents, date, author, subject, refs —
    pipe-delimited, newest first, `--topo-order` so parents never precede
    all of their children in the stream `assign_lanes` consumes."""
    fmt = "%H|%P|%ad|%an|%s|%D"
    out = subprocess.run(
        ["git", "log", "--all", "--topo-order", f"--since={since}",
         f"--format={fmt}", "--date=iso-strict"],
        cwd=repo, check=True, capture_output=True, text=True,
    ).stdout
    commits = []
    for line in out.splitlines():
        h, p, ad, an, s, *rest = line.split("|", 5) + [""]
        refs = rest[0] if rest else ""
        commits.append({
            "h": h,
            "parents": p.split() if p else [],
            "date": ad,
            "author": an,
            "msg": s,
            "refs": [r.strip() for r in refs.split(",") if r.strip()],
        })
    return commits


def assign_lanes(commits: list[dict]) -> tuple[list[int], list[tuple]]:
    """Assign each commit a lane (an integer column), and return the parent
    edges as (child_index, child_lane, parent_index, parent_lane) tuples for
    drawing. `commits` must be newest-first, topologically ordered (a commit
    never appears before all of its children) — exactly what
    `git log --topo-order` gives you.

    The idea: `active` tracks, per open lane, which commit hash that lane is
    currently waiting to see next (i.e. the parent it's heading toward). When
    we reach a commit that some lane was waiting for, that commit continues
    in that lane. If more than one lane was waiting for the same commit (a
    merge point, seen from below), only one continues — the rest are edges
    terminating into it. A commit nobody was waiting for is a new tip: it
    opens a fresh lane. Once we know a commit's own lane, its first parent
    inherits that lane (the "main" line continues straight down); any extra
    parents (the other side of a merge) each open their own new lane.
    """
    idx_by_hash = {c["h"]: i for i, c in enumerate(commits)}
    active: dict[int, str] = {}
    free_lanes: list[int] = []
    next_lane = 0
    lane_of = [None] * len(commits)
    edges: list[tuple] = []

    def alloc_lane() -> int:
        nonlocal next_lane
        if free_lanes:
            return free_lanes.pop(0)
        next_lane += 1
        return next_lane - 1

    for i, c in enumerate(commits):
        h = c["h"]
        my_lane = None
        for lane, waiting_for in list(active.items()):
            if waiting_for != h:
                continue
            if my_lane is None:
                my_lane = lane
            else:
                # A second lane converges on the same commit — a merge,
                # seen from below. It terminates here rather than continuing.
                edges.append((None, None, i, lane))
                del active[lane]
                free_lanes.append(lane)

        if my_lane is None:
            my_lane = alloc_lane()
        else:
            del active[my_lane]

        lane_of[i] = my_lane

        parents = c["parents"]
        if not parents:
            free_lanes.append(my_lane)
        else:
            active[my_lane] = parents[0]
            already_tracked = set(active.values())
            for extra_parent in parents[1:]:
                if extra_parent in idx_by_hash and extra_parent not in already_tracked:
                    active[alloc_lane()] = extra_parent

    for i, c in enumerate(commits):
        for p in c["parents"]:
            if p in idx_by_hash:
                j = idx_by_hash[p]
                edges.append((i, lane_of[i], j, lane_of[j]))

    return lane_of, [e for e in edges if e[0] is not None]


HTML_TEMPLATE = """<!doctype html>
<meta charset="utf-8">
<title>Commit graph</title>
<style>
:root{
  --bg:#0b0e14; --surface:#131826; --surface2:#171d2c; --line:#232a3d;
  --text:#d7dce6; --text-dim:#838fa8; --text-dimmer:#576079;
  --mono:ui-monospace,"SF Mono","Cascadia Code","JetBrains Mono",Consolas,monospace;
  --sans:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;
  --l0:#3d8bfd; --l1:#f2789c; --l2:#3fb950; --l3:#e2a23d; --l4:#8b7fd9; --l5:#41c9c1;
}
@media (prefers-color-scheme: light){
  :root{
    --bg:#f4f6fa; --surface:#ffffff; --surface2:#eef1f7; --line:#dde1ea;
    --text:#19212f; --text-dim:#5c6478; --text-dimmer:#8890a3;
    --l0:#2f6fe0; --l1:#d1487b; --l2:#2c9645; --l3:#b9822a; --l4:#6a5cc4; --l5:#1ea89f;
  }
}
*{box-sizing:border-box;}
body{margin:0; background:var(--bg); color:var(--text); font-family:var(--sans);}
.wrap{max-width:980px; margin:0 auto; padding:32px 16px 60px;}
h1{font-family:var(--mono); font-size:20px; margin:0 0 4px;}
.sub{color:var(--text-dim); font-size:13px; margin:0 0 20px;}
.graph{position:relative; background:var(--surface); border:1px solid var(--line); border-radius:8px; padding:14px 0;}
.graph svg{position:absolute; top:0; left:0; pointer-events:none;}
.rows{position:relative;}
.row{position:relative; display:flex; align-items:center; min-height:52px; padding:2px 16px 2px 0;}
.row:hover{background:var(--surface2);}
.gutter{flex-shrink:0;}
.body{min-width:0; flex:1; padding:5px 0;}
.msg{font-size:13px; color:var(--text); line-height:1.4; display:flex; align-items:baseline; gap:8px; flex-wrap:wrap;}
.meta{font-family:var(--mono); font-size:10.5px; color:var(--text-dimmer); margin-top:2px;}
.meta .hash{color:var(--text-dim);}
.ref{font-family:var(--mono); font-size:10px; font-weight:600; padding:2px 7px; border-radius:9px; border:1px solid; white-space:nowrap;}
.ref.head{background:var(--text); color:var(--bg); border-color:var(--text);}
</style>
<div class="wrap">
  <h1>__TITLE__</h1>
  <p class="sub">__SUBTITLE__</p>
  <div class="graph" id="graph"><svg id="svg"></svg><div class="rows" id="rows"></div></div>
</div>
<script>
const DATA = __DATA__;
const ROW_H = 52, LANE_W = 26, R = 5;
const GUTTER_W = DATA.lanes * LANE_W + 20;
const cs = getComputedStyle(document.documentElement);
const laneColors = ['--l0','--l1','--l2','--l3','--l4','--l5'];
const colorOf = lane => cs.getPropertyValue(laneColors[lane % laneColors.length]).trim();
const rowsEl = document.getElementById('rows'), svg = document.getElementById('svg');
const totalH = DATA.commits.length * ROW_H;
svg.setAttribute('width', GUTTER_W); svg.setAttribute('height', totalH);
svg.setAttribute('viewBox', `0 0 ${GUTTER_W} ${totalH}`);
function pos(i, lane){ return [16 + lane*LANE_W, i*ROW_H + ROW_H/2]; }
let paths = '';
DATA.edges.forEach(([i, laneI, j, laneJ])=>{
  const [x1,y1] = pos(i, laneI), [x2,y2] = pos(j, laneJ);
  const col = colorOf(Math.max(laneI, laneJ));
  if (laneI === laneJ) paths += `<line x1="${x1}" y1="${y1}" x2="${x2}" y2="${y2}" stroke="${col}" stroke-width="2" opacity="0.8"/>`;
  else { const midY=(y1+y2)/2; paths += `<path d="M ${x1} ${y1} C ${x1} ${midY}, ${x2} ${midY}, ${x2} ${y2}" fill="none" stroke="${col}" stroke-width="2" opacity="0.8"/>`; }
});
svg.innerHTML = paths;
DATA.commits.forEach((c, i)=>{
  const [hash, lane, date, author, msg, refs] = c;
  const [cx, cy] = pos(i, lane), col = colorOf(lane);
  const dot = document.createElementNS('http://www.w3.org/2000/svg','circle');
  dot.setAttribute('cx',cx); dot.setAttribute('cy',cy); dot.setAttribute('r',R);
  dot.setAttribute('fill',col); dot.setAttribute('stroke','var(--surface)'); dot.setAttribute('stroke-width','2');
  svg.appendChild(dot);
  const row = document.createElement('div'); row.className='row'; row.style.height=ROW_H+'px';
  const gutter = document.createElement('div'); gutter.className='gutter'; gutter.style.width=GUTTER_W+'px';
  row.appendChild(gutter);
  const body = document.createElement('div'); body.className='body';
  const msgLine = document.createElement('div'); msgLine.className='msg';
  let badges = '';
  refs.forEach(r=>{
    const clean = r.replace('HEAD -> ','').replace('origin/','').replace('tag: ','');
    const isHead = r.startsWith('HEAD');
    badges += `<span class="ref${isHead?' head':''}" style="${isHead?'':'color:'+col+';border-color:'+col}">${clean.length>28?clean.slice(0,27)+'…':clean}</span>`;
  });
  msgLine.innerHTML = badges + `<span>${msg.replace(/</g,'&lt;')}</span>`;
  body.appendChild(msgLine);
  const meta = document.createElement('div'); meta.className='meta';
  meta.innerHTML = `<span class="hash">${hash}</span> · ${author} · ${date.slice(0,16).replace('T',' ')}`;
  body.appendChild(meta);
  row.appendChild(body); rowsEl.appendChild(row);
});
</script>
"""


def render_html(commits: list[dict], lane_of: list[int], edges: list[tuple],
                 title: str, subtitle: str) -> str:
    lanes = (max(lane_of) + 1) if lane_of else 1
    data = {
        "commits": [
            [c["h"][:7], lane_of[i], c["date"], c["author"], c["msg"], c["refs"]]
            for i, c in enumerate(commits)
        ],
        "edges": edges,
        "lanes": lanes,
    }
    html = HTML_TEMPLATE
    html = html.replace("__TITLE__", title)
    html = html.replace("__SUBTITLE__", subtitle)
    html = html.replace("__DATA__", json.dumps(data))
    return html


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", type=Path, default=Path.cwd())
    ap.add_argument("--since", default="4 days ago")
    ap.add_argument("--out", type=Path, default=Path("commit-graph.html"))
    args = ap.parse_args()

    try:
        commits = run_git_log(args.repo, args.since)
    except subprocess.CalledProcessError as e:
        sys.exit(f"git log failed: {e.stderr}")

    if not commits:
        sys.exit(f"No commits found since {args.since!r} — widen --since.")

    lane_of, edges = assign_lanes(commits)
    lanes = max(lane_of) + 1
    title = f"{args.repo.name} — commit graph"
    subtitle = f"{len(commits)} commits, {lanes} lanes, since {args.since} · git log --all --topo-order"
    html = render_html(commits, lane_of, edges, title, subtitle)

    args.out.write_text(html, encoding="utf-8")
    print(f"{len(commits)} commits, {lanes} lanes -> {args.out}")


if __name__ == "__main__":
    main()
