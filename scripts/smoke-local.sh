#!/bin/sh
# Local smoke tests for flat-cyborg — deterministic, no network, no LLM.
#
# Exercises the wrapper against `sh`/`cat` targets so it can run anywhere
# (including CI). Verifies: ANSI stripping, orchestrator command typing,
# watchdog timeout, --cwd, usage errors, and the version subcommand.
#
# Usage:
#   scripts/smoke-local.sh [path-to-flat-cyborg]
# Defaults to ./target/debug/flat-cyborg, then a flat-cyborg on PATH.
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

# check NAME EXPECTED ACTUAL
check() {
  if [ "$2" = "$3" ]; then
    printf 'ok   %s\n' "$1"
    pass=$((pass + 1))
  else
    printf 'FAIL %s\n     expected: %s\n     actual:   %s\n' "$1" "$2" "$3"
    fail=$((fail + 1))
  fi
}

# S1: capture mode strips ANSI escape sequences.
out=$(printf '' | "$BIN" -- sh -c 'printf "\033[31mRED\033[0m done\n"')
check "S1 ansi-strip" "RED done" "$out"

# S2: orchestrator types commands into an interactive shell.
out=$(printf '' | "$BIN" --idle-ms 300 --timeout-ms 8000 \
  --cmd 'echo smoke-ok' --cmd 'exit 0' -- sh -i 2>/dev/null \
  | grep -c smoke-ok)
# echo + the shell echoing the typed command => the marker appears at least once.
if [ "$out" -ge 1 ]; then check "S2 orchestrator" "ge1" "ge1"; else check "S2 orchestrator" "ge1" "$out"; fi

# S3: watchdog aborts a hung target with exit code 124.
printf '' | "$BIN" --idle-ms 300 --timeout-ms 1500 -- sh -c 'sleep 30' >/dev/null 2>&1
check "S3 watchdog-124" "124" "$?"

# S4: --cwd runs the target in the given directory.
out=$(printf '' | "$BIN" --cwd /tmp -- sh -c 'pwd')
check "S4 cwd" "/tmp" "$out"

# S5: a nonexistent --cwd is a usage error (exit 2).
printf '' | "$BIN" --cwd /no-such-dir-xyz -- sh -c 'true' >/dev/null 2>&1
check "S5 cwd-missing-2" "2" "$?"

# S6: the version subcommand prints a version line.
out=$("$BIN" version | grep -c '^flat-cyborg ')
check "S6 version" "1" "$out"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
