#!/usr/bin/env bash
# lwk benchmarks: lwk against the commands an agent would otherwise run.
#
#   bench/run.sh                 quick: lanework-public (+ its live checkout), about a minute
#   BENCH_FULL=1 bench/run.sh    adds the vscode corpus, writes ../BENCHMARKS.md
#   JOBS=grep,startup bench/run.sh   only some jobs (see below)
#
# Needs: hyperfine git grep rg fd fzf ast-grep sqlite3 (and cargo, python3).
#
# What it does
#   1. takes a lock (bench/.corpus/.lock): two runs can never overlap;
#   2. checks the tools exist and records their versions;
#   3. clones the corpora at pinned commits into bench/.corpus/
#        lanework-public       local checkout, cloned (never modified), pinned commit
#        vscode                microsoft/vscode, shallow, pinned tag (BENCH_FULL=1 only)
#      and uses one corpus in place, read-only:
#        lanework-public-live  the working checkout itself, with its
#                              node_modules and dist, so the Noise column
#                              shows what a tool wades through there;
#   4. builds lwk in release mode;
#   5. per job runs hyperfine warm (and cold, see below), saving the JSON in
#      bench/results/, and saves each command's output once so bench/report.py
#      can count results, bytes, ~tokens (bytes / 4) and noise (hits under
#      node_modules, dist, .git, vendor, .next, coverage, target);
#   6. runs bench/report.py on whatever jobs ran.
#
# Jobs (JOBS=a,b takes group names or full ids, e.g. `grep` or `grep-vscode`)
#   find grep sg ls   per corpus      startup   lwk's pass-through
#   serve             per corpus: repeated find_files and list_files through one
#                     `lwk serve` (bench/serve_bench.py), median and p90 per query,
#                     set against the CLI warm-cache numbers of the find and ls jobs
#   sessions          always last: lwk against grep -r over past chats
# They run in that order, sessions last, because it is the one that reads the most.
#
# Past chats (the sessions job)
#   By default lwk searches a synthetic home, bench/.corpus/sessions-home,
#   written by bench/gen_sessions.py (fixed seed, a few tens of MB, folders
#   inside the corpus folder so the search is scoped): HOME points there for
#   lwk, with the CLAUDE_CONFIG_DIR, CODEX_HOME and OpenCode variables unset.
#   BENCH_REAL_SESSIONS=1 uses the real home instead, one run, no warm-up;
#   `grep -r` over ~/.claude and ~/.codex can take minutes there.
#
# Runs
#   Slow commands use hyperfine's adaptive runs (--min-runs 3 --max-runs 10,
#   so it stops at about 3 s of timing); the start-up jobs, which take
#   milliseconds, use fixed counts.
#
# Warm vs cold
#   Warm: the files are in the OS page cache (hyperfine --warmup runs first).
#   Cold: the page cache is emptied before every run (hyperfine --prepare).
#   Only with BENCH_FULL=1, and that needs root, so it runs only where this
#   works without a prompt:
#     macOS  `sudo -n purge`
#     Linux  `sudo -n sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'`
#            (or the same as root)
#   Run `sudo -v` first (or run as root) to get cold numbers; otherwise
#   cold is skipped and BENCHMARKS.md says so.
#
# The file-list cache (find, ls)
#   lwk caches the walked file list on disk (src/engine/cache.rs). The find
#   and ls jobs therefore time two variants: `lwk` with the cache warm (the
#   hyperfine warm-up fills it) and `lwk-nocache` with LWK_NO_CACHE=1, which
#   reads and writes nothing. XDG_CACHE_HOME points at bench/.corpus/xdg-cache,
#   so the real cache (~/Library/Caches/lwk, ~/.cache/lwk) is never touched;
#   that folder is emptied at the start of every run.
#
# lwk is run in-process: LANEWORK_HOME points at an empty folder, so it never
# talks to a running Lanework app's kit socket (which would answer from a
# warm index). Every run of `lwk find` therefore builds its index from
# scratch, which is the cost the app's warm index avoids.
#
# Output
#   BENCH_FULL=1, all jobs   ../BENCHMARKS.md
#   anything else            bench/results/BENCHMARKS.md (a partial or quick
#                            report never overwrites the full one)
#   REPORT=path              writes there instead
#
# Environment overrides
#   LANEWORK_PUBLIC   the checkout to clone and use live (default ~/fuongz_projects/lanework-public)
#   LP_COMMIT         pinned commit of it
#   VSCODE_TAG        pinned vscode tag
#   WARMUP            warm-up runs (default 1)
#   MIN_RUNS MAX_RUNS adaptive bounds (default 3 / 10)
#   SERVE_RUNS        requests per query in the serve job (default 200)
#   SLOW_S            a command whose first run takes this long (default 5 s)
#                     is timed once (that first run, which also saves its output)
#   BENCH_LIVE_GREP=1 also run plain `grep -r` in the live checkout: it
#                     reads every node_modules file, about 3 minutes a run here
#   COLD_RUNS         cold runs (default 3)
#   SESSIONS_QUERY    what to look for in past chats (default useEffect)
#   SESSIONS_MB       size of the synthetic chat home (default 30)
#   SKIP_BUILD=1      reuse target/release/lwk
set -euo pipefail

BENCH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$BENCH")"
CORPUS="$BENCH/.corpus"
RES="$BENCH/results"

LANEWORK_PUBLIC="${LANEWORK_PUBLIC:-$HOME/fuongz_projects/lanework-public}"
LP_COMMIT="${LP_COMMIT:-3619bf12a657573eacd8b65dd0fe78e477e12a68}"
VSCODE_TAG="${VSCODE_TAG:-1.105.0}"
WARMUP="${WARMUP:-1}"
MIN_RUNS="${MIN_RUNS:-3}"
MAX_RUNS="${MAX_RUNS:-10}"
COLD_RUNS="${COLD_RUNS:-3}"
SLOW_S="${SLOW_S:-5}"
SERVE_RUNS="${SERVE_RUNS:-200}"
SESSIONS_QUERY="${SESSIONS_QUERY:-useEffect}"
SESSIONS_MB="${SESSIONS_MB:-30}"
FULL="${BENCH_FULL:-}"
REAL_SESSIONS="${BENCH_REAL_SESSIONS:-}"
JOBS="${JOBS:-}"
START_S=$SECONDS

# 0. lock --------------------------------------------------------------
# mkdir is atomic; the pid inside tells a stale lock from a live one.
mkdir -p "$CORPUS"
LOCK="$CORPUS/.lock"
if ! mkdir "$LOCK" 2>/dev/null; then
  other="$(cat "$LOCK/pid" 2>/dev/null || true)"
  if [[ -n "$other" ]] && kill -0 "$other" 2>/dev/null; then
    echo "bench/run.sh is already running (pid $other); not starting a second one." >&2
    exit 1
  fi
  echo "removing the stale lock of pid ${other:-?}" >&2
  rm -rf "$LOCK"
  mkdir "$LOCK" || { echo "could not take $LOCK" >&2; exit 1; }
fi
echo $$ > "$LOCK/pid"
trap 'rm -rf "$LOCK"' EXIT
trap 'exit 130' INT TERM

# 1. tools -------------------------------------------------------------
missing=()
for t in hyperfine git grep rg fd fzf ast-grep sqlite3 cargo python3; do
  command -v "$t" >/dev/null 2>&1 || missing+=("$t")
done
if ((${#missing[@]})); then
  echo "missing tools: ${missing[*]}" >&2
  echo "install with Homebrew: brew install hyperfine ripgrep fd fzf ast-grep sqlite" >&2
  exit 1
fi
first_line() { "$@" 2>&1 | head -n1; }

# Which jobs run: JOBS is a list of group names or ids.
selected() {
  [[ -z "$JOBS" ]] && return 0
  local id="$1" want
  IFS=',' read -ra want <<< "$JOBS"
  for w in "${want[@]}"; do
    w="${w// /}"
    [[ "$w" == "$id" || "$id" == "$w"-* ]] && return 0
  done
  return 1
}

# Start from an empty results folder.
mkdir -p "$RES"
find "${RES:?}" -mindepth 1 -delete

echo "== tools"
TOOLS="$RES/tools.txt"
{
  echo "tool.hyperfine=$(first_line hyperfine --version)"
  echo "tool.git=$(first_line git --version)"
  echo "tool.grep=$(first_line grep -V)"
  echo "tool.rg=$(first_line rg --version)"
  echo "tool.fd=$(first_line fd --version)"
  echo "tool.fzf=$(first_line fzf --version)"
  echo "tool.ast-grep=$(first_line ast-grep --version)"
  echo "tool.sqlite3=$(first_line sqlite3 --version | cut -d' ' -f1-2)"
  echo "tool.find=$(uname -s) find"
} > "$TOOLS"
sed 's/^tool\./  /' "$TOOLS"

# 2. corpora -----------------------------------------------------------
[[ -d "$LANEWORK_PUBLIC/.git" ]] || { echo "no checkout at $LANEWORK_PUBLIC" >&2; exit 1; }
if [[ ! -d "$CORPUS/lanework-public/.git" ]]; then
  git clone -q --no-hardlinks "$LANEWORK_PUBLIC" "$CORPUS/lanework-public"
fi
git -C "$CORPUS/lanework-public" checkout -q --detach "$LP_COMMIT"
LP="$(cd "$CORPUS/lanework-public" && pwd -P)"
LIVE="$(cd "$LANEWORK_PUBLIC" && pwd -P)"   # used in place: read-only, never written to
VS=""
if [[ -n "$FULL" ]]; then
  if [[ ! -d "$CORPUS/vscode/.git" ]]; then
    git clone -q --depth 1 --branch "$VSCODE_TAG" https://github.com/microsoft/vscode "$CORPUS/vscode"
  fi
  VS="$(cd "$CORPUS/vscode" && pwd -P)"
fi
files() { git -C "$1" ls-files | wc -l | tr -d ' '; }

# 3. lwk ---------------------------------------------------------------
if [[ -z "${SKIP_BUILD:-}" ]]; then
  echo "== cargo build --release"
  (cd "$ROOT" && cargo build --release -q)
fi
LWK="$ROOT/target/release/lwk"
[[ -x "$LWK" ]] || { echo "no $LWK" >&2; exit 1; }
NOAPP="$CORPUS/no-app"
mkdir -p "$NOAPP"
export LANEWORK_HOME="$NOAPP"   # empty on purpose: never ask the app
# lwk's file-list cache goes inside the corpus folder, emptied for every run.
XDG_CACHE_HOME="$CORPUS/xdg-cache"
rm -rf "${XDG_CACHE_HOME:?}"
mkdir -p "$XDG_CACHE_HOME"
export XDG_CACHE_HOME

# cold cache -----------------------------------------------------------
COLD_CMD=""
if [[ -n "$FULL" ]]; then
  case "$(uname -s)" in
    Darwin) command -v purge >/dev/null && sudo -n purge >/dev/null 2>&1 && COLD_CMD="sudo -n purge" ;;
    Linux)
      if [[ $EUID -eq 0 && -w /proc/sys/vm/drop_caches ]]; then COLD_CMD="sync; echo 3 > /proc/sys/vm/drop_caches"
      elif sudo -n true >/dev/null 2>&1; then COLD_CMD="sudo -n sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'"; fi ;;
  esac
  if [[ -n "$COLD_CMD" ]]; then echo "== cold runs: on ($COLD_CMD)"; else echo "== cold runs: off (no passwordless purge / drop_caches; see header)"; fi
else
  echo "== cold runs: off (quick run)"
fi

# 4. the chats lwk searches -----------------------------------------------
# A synthetic home, scoped to the lanework-public clone; or the real one.
SESSIONS_CWD="$LP"
if [[ -n "$REAL_SESSIONS" ]]; then
  SESSIONS_HOME="$HOME"
  SESSIONS_CWD="$(dirname "$LIVE")"
  SESSIONS_DESC="the real ~/.claude and ~/.codex (BENCH_REAL_SESSIONS=1)"
else
  SESSIONS_HOME="$CORPUS/sessions-home"
  SESSIONS_DESC="a synthetic home of $SESSIONS_MB MB (bench/gen_sessions.py), scoped to the lanework-public folder"
  if selected sessions; then
    stamp="$(cat "$SESSIONS_HOME/.stamp" 2>/dev/null || true)"
    want="$(shasum "$BENCH/gen_sessions.py" | cut -c1-12)-$SESSIONS_MB-$LP"
    if [[ "$stamp" != "$want" ]]; then
      echo "== sessions fixture"
      python3 -I "$BENCH/gen_sessions.py" "$SESSIONS_HOME" "$LP" --mb "$SESSIONS_MB"
      echo "$want" > "$SESSIONS_HOME/.stamp"
    fi
  fi
fi
# lwk's home is $HOME; Claude, Codex and OpenCode each have an override
# variable, which must not lead out of the fixture.
SESS_ENV="env -u CLAUDE_CONFIG_DIR -u CODEX_HOME -u OPENCODE_DATA_DIR -u OPENCODE_APPNAME -u XDG_DATA_HOME HOME=$(printf '%q' "$SESSIONS_HOME")"
[[ -n "$REAL_SESSIONS" ]] && SESS_ENV=""

{
  cat "$TOOLS"
  echo "date=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "os=$(uname -srm)"
  case "$(uname -s)" in
    Darwin)
      echo "cpu=$(sysctl -n machdep.cpu.brand_string)"
      echo "cores=$(sysctl -n hw.ncpu)"
      echo "ram_gb=$(( $(sysctl -n hw.memsize) / 1073741824 ))"
      echo "macos=$(sw_vers -productVersion)" ;;
    Linux)
      echo "cpu=$(awk -F: '/model name/ {gsub(/^ /,"",$2); print $2; exit}' /proc/cpuinfo)"
      echo "cores=$(nproc)"
      echo "ram_gb=$(awk '/MemTotal/ {printf "%d", $2/1048576}' /proc/meminfo)" ;;
  esac
  echo "cache.dir=bench/.corpus/xdg-cache (XDG_CACHE_HOME, emptied at the start of the run)"
  echo "lwk=$("$LWK" version)"
  echo "lwk.commit=$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  echo "corpus.lanework-public=$(git -C "$LP" rev-parse HEAD) ($(files "$LP") tracked files), clone"
  live_dirty="$(git -C "$LIVE" status --porcelain --untracked-files=no 2>/dev/null | wc -l | tr -d ' ')"
  echo "corpus.lanework-public-live=working checkout at $(git -C "$LIVE" rev-parse --short HEAD) ($(files "$LIVE") tracked files, $live_dirty modified), used in place, read-only; node_modules and dist included"
  [[ -n "$VS" ]] && echo "corpus.vscode=$(git -C "$VS" rev-parse HEAD) tag $VSCODE_TAG, shallow ($(files "$VS") tracked files)"
  echo "mode=$([[ -n "$FULL" ]] && echo full || echo quick)"
  echo "runs.warm=adaptive, $MIN_RUNS to $MAX_RUNS runs (warm-up $WARMUP); start-up jobs 100 and 50 runs"
  echo "runs.warmup=$WARMUP"
  echo "runs.cold=$COLD_RUNS"
  echo "runs.serve=$SERVE_RUNS requests per query, one at a time, after one warm-up request"
  echo "cold=${COLD_CMD:+yes}"
  echo "cold.cmd=$COLD_CMD"
  echo "sessions.home=$SESSIONS_DESC"
  [[ -n "$REAL_SESSIONS" ]] && echo "sessions.real=yes"
  echo "jobs=${JOBS:-all}"
} > "$RES/meta.txt"
rm -f "$TOOLS"

# 5. jobs --------------------------------------------------------------
now() { python3 -c 'import time; print(time.time())'; }

# job ID TITLE DIR KIND DESC RUN_MODE FLAGS -- LABEL CMD [LABEL CMD ...]
#   RUN_MODE  adaptive | single | N,M (fixed warm runs N, cold runs M)
job() {
  local id="$1" title="$2" dir="$3" kind="$4" desc="$5" mode="$6" flags="$7"
  shift 8
  selected "$id" || return 0
  printf '%s\t%s\t%s\t%s\t%s\n' "$id" "$title" "$(basename "$dir")" "$kind" "$desc" >> "$RES/jobs.tsv"
  echo "== $id"
  mkdir -p "$RES/out/$id"
  local args=() slow=() label cmd t0 took
  # hyperfine 2 wants the shell chosen out loud when a command has pipes.
  [[ -n "$flags" ]] || flags="--shell=default"
  while (($#)); do
    label="$1"; cmd="$2"; shift 2
    # This first run saves the output; its time decides whether the command
    # is slow (SLOW_S seconds or more): a slow one is timed once, not 3 to 10 times.
    t0=$(now)
    (cd "$dir" && bash -c "$cmd" > "$RES/out/$id/$label.out" 2> "$RES/out/$id/$label.err") || true
    took=$(python3 -c "print($(now) - $t0)")
    if [[ "$mode" == adaptive ]] && python3 -c "raise SystemExit(not $took >= $SLOW_S)"; then
      slow+=("$label" "$took"); printf "   %s: %.1fs, timed once (the run that saved its output)\n" "$label" "$took"
    else
      args+=(-n "$label" "$cmd")
    fi
    if [[ "$label" == ast-grep* ]]; then   # JSON, for counting matches and ranges
      (cd "$dir" && bash -c "${cmd/ run / run --json=stream }" > "$RES/out/$id/$label.json" 2>/dev/null) || true
    fi
  done
  local warm_runs cold_runs warmup="$WARMUP"
  case "$mode" in
    adaptive) warm_runs=(--min-runs "$MIN_RUNS" --max-runs "$MAX_RUNS"); cold_runs=(--runs "$COLD_RUNS") ;;
    single)   warm_runs=(--runs 1); cold_runs=(--runs 1); warmup=0 ;;
    *)        warm_runs=(--runs "${mode%,*}"); cold_runs=(--runs "${mode#*,}") ;;
  esac
  # shellcheck disable=SC2086  # $flags is a word list on purpose
  if ((${#args[@]})); then
    (cd "$dir" && hyperfine -i $flags --warmup "$warmup" "${warm_runs[@]}" \
       --export-json "$RES/$id.warm.json" "${args[@]}" > "$RES/$id.warm.log" 2>&1) \
       || echo "hyperfine failed for $id (warm), see $RES/$id.warm.log" >&2
    if [[ -n "$COLD_CMD" && "$mode" != single ]]; then
      # shellcheck disable=SC2086
      (cd "$dir" && hyperfine -i $flags "${cold_runs[@]}" --prepare "$COLD_CMD" \
         --export-json "$RES/$id.cold.json" "${args[@]}" > "$RES/$id.cold.log" 2>&1) \
         || echo "hyperfine failed for $id (cold), see $RES/$id.cold.log" >&2
    fi
  fi
  if ((${#slow[@]})); then
    # Not run again: that first run is the one timing, in hyperfine's JSON shape.
    python3 -I - "$RES/$id.warm-slow.json" "${slow[@]}" <<'PY'
import json, sys
out, rest = sys.argv[1], sys.argv[2:]
results = [{"name": rest[i], "times": [float(rest[i + 1])]} for i in range(0, len(rest), 2)]
json.dump({"results": results}, open(out, "w"))
PY
  fi
}

# Quoted for use inside the command strings: the checkout lives in a folder
# whose name has a space, and hyperfine -N splits on spaces too.
Q="\"$LWK\""

# Jobs find, grep, sg, ls run on every corpus; the query differs by corpus.
corpora=(lanework-public lanework-public-live)
[[ -n "$VS" ]] && corpora+=(vscode)
for corpus in "${corpora[@]}"; do
  case $corpus in
    lanework-public)      D="$LP" ;;
    lanework-public-live) D="$LIVE" ;;
    vscode)               D="$VS" ;;
  esac
  if [[ $corpus == vscode ]]; then
    TYPO=terminalservce; WORD=registerCommand; PAT='setTimeout($A, 0)'; RE='setTimeout\(.*, *0\)'
  else
    TYPO=convview; WORD=useEffect; PAT='useEffect($A, [])'; RE='useEffect\(.*\[\]\)'
  fi

  job "find-$corpus" "Misspelled file name" "$D" paths "query \`$TYPO\`" adaptive "" -- \
    lwk "$Q find $TYPO" \
    lwk-nocache "LWK_NO_CACHE=1 $Q find $TYPO" \
    find "find . -iname '*$TYPO*'" \
    fd "fd -i $TYPO" \
    fzf "git ls-files | fzf --filter $TYPO"

  GREPR=(grep "grep -rniF $WORD .")
  [[ $corpus == lanework-public-live && -z "${BENCH_LIVE_GREP:-}" ]] && GREPR=()
  job "grep-$corpus" "Text search" "$D" grep "literal \`$WORD\`, case-insensitive" adaptive "" -- \
    lwk "$Q grep -F $WORD" \
    ${GREPR[@]+"${GREPR[@]}"} \
    git-grep "git grep -niF $WORD" \
    rg "rg -niF $WORD ."

  job "sg-$corpus" "Structural search" "$D" sg "pattern \`$PAT\`; regex attempt \`$RE\`" adaptive "" -- \
    lwk "$Q sg '$PAT'" \
    ast-grep "ast-grep run -p '$PAT' ." \
    grep-regex "grep -rnE --include='*.ts' --include='*.tsx' --include='*.js' --include='*.jsx' '$RE' ."

  job "ls-$corpus" "List files" "$D" paths "every file the tool lists" adaptive "" -- \
    lwk "$Q ls" \
    lwk-nocache "LWK_NO_CACHE=1 $Q ls" \
    lwk-all "$Q ls --limit 50000" \
    find "find . -type f" \
    fd "fd -t f" \
    rg "rg --files"
done

# 6. start-up (in the clone: `git status` may refresh the index of a checkout,
# and the live one is never written to)
job "startup-true" "Start-up: a no-op" "$LP" startup "\`lwk true\` runs \`true\` through lwk's pass-through" 100,20 "-N" -- \
  lwk "$Q true" \
  true "true"
job "startup-git" "Start-up: git status" "$LP" startup "\`lwk git status\` runs \`git status\` through lwk's pass-through" 50,10 "-N" -- \
  lwk "$Q git status" \
  git "git status"

# 7. lwk serve: one long-lived process, the same tools over stdin/stdout. The
# driver sends each query once to warm it, then SERVE_RUNS times, one at a time.
# Same XDG_CACHE_HOME and LANEWORK_HOME as above (exported). The checkouts the
# CLI jobs use are the ones timed here, so the numbers sit side by side.
if selected serve; then
  echo "== serve"
  for corpus in lanework-public ${VS:+vscode}; do
    if [[ $corpus == vscode ]]; then D="$VS"; TYPO=terminalservce; ONE=registerCommand; MULTI="terminal service"
    else D="$LP"; TYPO=convview; ONE=transcript; MULTI="codex transcript"; fi
    python3 -I "$BENCH/serve_bench.py" --lwk "$LWK" --tree "$D" --runs "$SERVE_RUNS" \
      --out "$RES/serve-$corpus.json" --git --find "typo=$TYPO" --find "one-word=$ONE" --find "multi-word=$MULTI"
  done
fi

# 8. past chats, last: it reads the most.
if [[ -n "$REAL_SESSIONS" ]]; then
  job "sessions" "Past chats" "$SESSIONS_CWD" sessions \
    "query \`$SESSIONS_QUERY\` over $SESSIONS_DESC, from \`$(basename "$SESSIONS_CWD")\`; one run, no warm-up" single "" -- \
    lwk "$SESS_ENV $Q sessions $SESSIONS_QUERY" \
    grep-r "grep -rniF $SESSIONS_QUERY \"$HOME/.claude/projects\" \"$HOME/.codex/sessions\""
else
  job "sessions" "Past chats" "$SESSIONS_CWD" sessions \
    "query \`$SESSIONS_QUERY\` over $SESSIONS_DESC" adaptive "" -- \
    lwk "$SESS_ENV $Q sessions $SESSIONS_QUERY" \
    grep-r "grep -rniF $SESSIONS_QUERY \"$SESSIONS_HOME/.claude/projects\" \"$SESSIONS_HOME/.codex/sessions\""
fi

echo "== report"
if [[ -n "${REPORT:-}" ]]; then DEST="$REPORT"
elif [[ -n "$FULL" && -z "$JOBS" ]]; then DEST="$ROOT/BENCHMARKS.md"
else DEST="$RES/BENCHMARKS.md"; fi
python3 -I "$BENCH/report.py" "$RES" "$DEST" "$((SECONDS - START_S))"
echo "== done in $((SECONDS - START_S)) s"
