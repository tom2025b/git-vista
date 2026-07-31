#!/usr/bin/env bash
# R9: prove each declarative escape-battery claim notices the mechanism it names.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
work_root=$(mktemp -d "${TMPDIR:-/tmp}/git-vista-mutation-matrix.XXXXXX")
trap 'rm -rf -- "$work_root"' EXIT

readonly -a mutants=(M1 M2 M3 M4 M5 M6 M7 M8 M9 M10 M11)
declare -Ar mutant_patch=(
  [M1]="ci/mutants/M1-apply-seccomp-empty.patch"
  [M2]="ci/mutants/M2-skip-landlock-restrict-self.patch"
  [M3]="ci/mutants/M3-empty-secret-excludes.patch"
  [M4]="ci/mutants/M4-remove-strict-unshare-net.patch"
  [M5]="ci/mutants/M5-strict-net-allow-default-ports.patch"
  [M6]="ci/mutants/M6-ignore-hooks-blocked-dir.patch"
  [M7]="ci/mutants/M7-widen-prctl-comparison.patch"
  [M8]="ci/mutants/M8-remove-af-unix-socket-rule.patch"
  [M9]="ci/mutants/M9-widen-af-unix-comparison.patch"
  [M10]="ci/mutants/M10-allow-io-uring.patch"
  [M11]="ci/mutants/M11-empty-ssh-known-hosts-carveout.patch"
)

failures=0

error() {
  printf 'mutation-matrix: ERROR: %s\n' "$*" >&2
  failures=$((failures + 1))
}

apply_mutant() {
  local tree=$1
  local patch_file=$2
  local -a strict_args=(--forward --fuzz=0 --batch)

  # BSD patch calls exact-context matching --strict. Ubuntu and this project
  # host use GNU patch 2.7, which has no --strict option; --fuzz=0 is its exact
  # matching equivalent. In either implementation a non-applying hunk exits
  # non-zero and, under set -e, fails the matrix instead of becoming a skip.
  if patch --help 2>&1 | grep -q -- '--strict'; then
    strict_args=(--forward --strict)
  fi

  (
    cd "$tree"
    patch -p1 "${strict_args[@]}" < "$repo_root/$patch_file"
  )
}

declarations="$work_root/declarations.tsv"
python3 - \
  "$repo_root/crates/git-vista-server/src/sandbox/escape_suite.rs" \
  "$repo_root/crates/git-vista-server/src/sandbox/hook_mode_suite.rs" \
  > "$declarations" <<'PY'
import pathlib
import re
import sys

# Upper bound is exclusive: range(1, 12) == M1..M11. This set is the third and
# most-missed registration site for a new mutant (the `mutants` array and the
# `mutant_patch` map above are the other two) — an unlisted id makes the parser
# reject the case that names it, with a message about an *unknown mutant* rather
# than about this line.
known = {f"M{i}" for i in range(1, 12)}
case_re = re.compile(
    r"const\s+CASE_[A-Z0-9_]+:\s*EscapeCase\s*=\s*EscapeCase\s*\{(.*?)\n\};",
    re.DOTALL,
)
seen = set()
for name in sys.argv[1:]:
    path = pathlib.Path(name)
    module = path.stem
    for match in case_re.finditer(path.read_text()):
        body = match.group(1)
        case_id = re.search(r'\bid:\s*"([^"]+)"', body)
        dies_under = re.search(r"\bdies_under:\s*&\[([^]]*)\]", body, re.DOTALL)
        if case_id is None or dies_under is None:
            raise SystemExit(f"{path}: case declaration lacks id or dies_under")
        case_id = case_id.group(1)
        if case_id in seen:
            raise SystemExit(f"duplicate EscapeCase id: {case_id}")
        seen.add(case_id)
        mutants = re.findall(r"MutantId::(M[0-9]+)", dies_under.group(1))
        unknown = sorted(set(mutants) - known)
        if unknown:
            raise SystemExit(f"{path}: {case_id} names unknown mutants: {unknown}")
        print(module, case_id, ",".join(mutants), sep="\t")

if not seen:
    raise SystemExit("no EscapeCase declarations found")
PY

mapfile -t case_rows < "$declarations"
declare -a case_ids=()
declare -a case_modules=()
declare -A case_mutants=()
declare -A mutant_named=()
for row in "${case_rows[@]}"; do
  IFS=$'\t' read -r module case_id declared_mutants <<< "$row"
  case_modules+=("$module")
  case_ids+=("$case_id")
  case_mutants["$case_id"]=$declared_mutants
  if [[ -z $declared_mutants ]]; then
    error "case $case_id declares an empty dies_under list"
    continue
  fi
  IFS=',' read -ra named <<< "$declared_mutants"
  for mutant in "${named[@]}"; do
    mutant_named["$mutant"]=1
  done
done

patch_count=$(find "$repo_root/ci/mutants" -maxdepth 1 -type f -name '*.patch' | wc -l)
if [[ $patch_count -ne ${#mutants[@]} ]]; then
  error "ci/mutants contains $patch_count patches; expected ${#mutants[@]}"
fi
for mutant in "${mutants[@]}"; do
  patch_file=${mutant_patch[$mutant]}
  if [[ ! -f "$repo_root/$patch_file" ]]; then
    error "$mutant patch is missing: $patch_file"
    continue
  fi
  changed_files=$(grep -c '^+++ b/' "$repo_root/$patch_file" || true)
  hunks=$(grep -c '^@@' "$repo_root/$patch_file" || true)
  if [[ $changed_files -ne 1 || $hunks -ne 1 ]]; then
    error "$mutant patch must contain exactly one file and one hunk: $patch_file"
  fi
done

# One snapshot of the source, taken once, before any tree is built.
#
# Every mutant tree is copied from this snapshot rather than from $repo_root,
# so the whole grid describes exactly one version of the code. Copying from
# the live repository per mutant — as this did originally — makes the grid
# non-atomic: an edit landing mid-run (a developer, an agent, or the 60s
# auto-checkpointer's own working tree) puts M0 and M5 on different sources,
# and a grid whose cells describe different trees cannot support any claim
# about which mechanism a case depends on. That silent incoherence is the
# same disease as a vacuous assertion, one level up.
pristine="$work_root/pristine"
mkdir -p "$pristine"
rsync -a \
  --exclude '/.git/' \
  --exclude '/target/' \
  "$repo_root/" "$pristine/"
printf 'mutation-matrix: source snapshot taken; the grid below describes it alone\n'

declare -A outcome=()
run_one_tree() {
  local label=$1
  local patch_file=${2:-}
  local tree="$work_root/tree-$label"
  local report="$work_root/report-$label.txt"
  local build_log="$work_root/build-$label.log"

  mkdir -p "$tree"
  rsync -a "$pristine/" "$tree/"

  if [[ -n $patch_file ]]; then
    printf 'mutation-matrix: applying %s (%s)\n' "$label" "$patch_file"
    apply_mutant "$tree" "$patch_file"
  else
    printf 'mutation-matrix: running %s (unmodified tree)\n' "$label"
  fi

  if ! (
    cd "$tree"
    CARGO_TARGET_DIR="$tree/target" cargo test -p git-vista-server --no-run
  ) > "$build_log" 2>&1; then
    printf 'mutation-matrix: %s did not build; tail follows\n' "$label" >&2
    tail -80 "$build_log" >&2
    exit 1
  fi

  : > "$report"
  for index in "${!case_ids[@]}"; do
    local case_id=${case_ids[$index]}
    local module=${case_modules[$index]}
    local test_name="sandbox::${module}::${case_id}"
    local test_log="$work_root/test-${label}-${case_id}.log"
    local status=0

    if (
      cd "$tree"
      GV_ESCAPE_REPORT="$report" \
        CARGO_TARGET_DIR="$tree/target" \
        cargo test -p git-vista-server "$test_name" -- --exact --test-threads=1
    ) > "$test_log" 2>&1; then
      status=0
    else
      status=$?
    fi

    local records
    records=$(grep -c "^GV-ESCAPE case=${case_id} " "$report" || true)
    if [[ $status -eq 0 && $records -eq 1 ]] && \
      grep -q "^GV-ESCAPE case=${case_id} result=contained " "$report"; then
      outcome["$label|$case_id"]=PASS
    else
      outcome["$label|$case_id"]=FAIL
      if [[ $records -ne 1 ]]; then
        printf 'mutation-matrix: %s/%s wrote %s report records (expected 1)\n' \
          "$label" "$case_id" "$records" >&2
        tail -40 "$test_log" >&2
      fi
    fi
    printf 'mutation-matrix: %-2s / %-28s %s\n' \
      "$label" "$case_id" "${outcome["$label|$case_id"]}"
  done
}

run_one_tree M0
for mutant in "${mutants[@]}"; do
  run_one_tree "$mutant" "${mutant_patch[$mutant]}"
done

printf '\nMUTANT'
for case_id in "${case_ids[@]}"; do
  printf '\t%s' "$case_id"
done
printf '\n'
for label in M0 "${mutants[@]}"; do
  printf '%s' "$label"
  for case_id in "${case_ids[@]}"; do
    printf '\t%s' "${outcome["$label|$case_id"]}"
  done
  printf '\n'
done
printf '\n'

for case_id in "${case_ids[@]}"; do
  if [[ ${outcome["M0|$case_id"]} != PASS ]]; then
    error "M0/$case_id must PASS"
  fi
  declared_mutants=${case_mutants[$case_id]}
  [[ -n $declared_mutants ]] || continue
  IFS=',' read -ra named <<< "$declared_mutants"
  for mutant in "${named[@]}"; do
    if [[ ${outcome["$mutant|$case_id"]} != FAIL ]]; then
      error "$mutant/$case_id is declared in dies_under but read ${outcome["$mutant|$case_id"]}"
    fi
  done
done

for mutant in "${mutants[@]}"; do
  if [[ -z ${mutant_named[$mutant]:-} ]]; then
    error "$mutant is not named by any case (mutant-to-case closure failed)"
  fi
done

if [[ $failures -ne 0 ]]; then
  printf 'mutation-matrix: FAILED with %d contract violation(s)\n' "$failures" >&2
  exit 1
fi

printf 'mutation-matrix: PASS — M0 all-pass, declared cells fail, closure holds both ways\n'
