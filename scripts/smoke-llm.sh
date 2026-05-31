#!/bin/sh
# LLM smoke tests for flat-cyborg — opt-in, NOT for CI.
#
# Drives real interactive LLM CLIs (claude, codex) through flat-cyborg and
# checks --extract / --auto-approve end to end. Requires the target CLI(s) to
# be installed, authenticated, and the current directory to be one they already
# trust (so no first-run trust menu). These runs are slow (real model replies)
# and non-deterministic in wording, so they assert structure, not exact prose.
#
# Usage:
#   scripts/smoke-llm.sh [path-to-flat-cyborg]
# Run from a git repo the agent trusts (e.g. the flat-cyborg checkout).
set -u

BIN="${1:-}"
if [ -z "$BIN" ]; then
  if [ -x ./target/debug/flat-cyborg ]; then
    BIN=./target/debug/flat-cyborg
  else
    BIN=flat-cyborg
  fi
fi

pass=0
fail=0
skip=0

check() { # NAME EXPECTED ACTUAL
  if [ "$2" = "$3" ]; then
    printf 'ok   %s\n' "$1"; pass=$((pass + 1))
  else
    printf 'FAIL %s\n     expected: %s\n     actual:   %s\n' "$1" "$2" "$3"; fail=$((fail + 1))
  fi
}

have() { command -v "$1" >/dev/null 2>&1; }

# extract_short CLI  -> asserts a one-word reply is captured cleanly.
extract_short() {
  cli="$1"
  out=$(timeout 150 "$BIN" --tui --extract --idle-ms 6000 --timeout-ms 140000 \
    --cmd 'reply with exactly one word: banana' -- "$cli" 2>/dev/null)
  check "$cli extract-short" "banana" "$out"
}

# extract_multiline CLI -> asserts 1..12 captured in full with no UI chrome.
extract_multiline() {
  cli="$1"
  out=$(timeout 170 "$BIN" --tui --extract --idle-ms 7000 --timeout-ms 160000 \
    --cmd 'list the numbers 1 to 12, each on its own line, just the number' \
    -- "$cli" 2>/dev/null)
  lines=$(printf '%s\n' "$out" | grep -cE '^[[:space:]]*[0-9]+$')
  chrome=$(printf '%s\n' "$out" | grep -cE '✻|❯|⏵|›|╭|│|gpt-|auto mode|weekly')
  if [ "$lines" -ge 12 ] && [ "$chrome" -eq 0 ]; then
    check "$cli extract-multiline" "12lines+0chrome" "12lines+0chrome"
  else
    check "$cli extract-multiline" "12lines+0chrome" "${lines}lines+${chrome}chrome"
  fi
}

# agentic CLI -> auto-approve a tool action, read back the branch name.
agentic_branch() {
  cli="$1"
  want=$(git branch --show-current 2>/dev/null)
  out=$(timeout 200 "$BIN" --tui --extract --auto-approve --idle-ms 12000 --timeout-ms 190000 \
    --cmd 'Run the shell command "git branch --show-current" and reply with ONLY the branch name it prints, nothing else.' \
    -- "$cli" 2>/dev/null)
  check "$cli agentic-branch" "$want" "$out"
}

for cli in claude codex; do
  if have "$cli"; then
    printf '== %s ==\n' "$cli"
    extract_short "$cli"
    extract_multiline "$cli"
    agentic_branch "$cli"
  else
    printf 'skip %s (not installed)\n' "$cli"
    skip=$((skip + 1))
  fi
done

printf '\n%d passed, %d failed, %d CLIs skipped\n' "$pass" "$fail" "$skip"
[ "$fail" -eq 0 ]
