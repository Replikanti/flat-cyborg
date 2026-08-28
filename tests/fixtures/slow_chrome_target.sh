#!/bin/sh
# Fake LLM CLI fixture for the #71 concurrency reproduction harness (M1).
#
# Emits chrome resembling an interactive LLM TUI (a banner box, an animated
# spinner with an "esc to interrupt" hint), then — after a configurable slow
# delay — reads the wrapped prompt from stdin, extracts the FCB_<tok>_BEGIN /
# FCB_<tok>_END sentinel pair (the same shape flat-cyborg emits, see
# src/main.rs:303 and the inline fixture at tests/cli.rs:343), and prints a
# fenced reply. NO live model, NO network: everything is local and deterministic,
# driven by env knobs so one script reproduces the slow-reply, marker-less and
# target-died shapes.
#
# Env knobs:
#   REPLY_DELAY_MS  delay before the answer is emitted, in ms (default 400)
#   OMIT_MARKER     if "1", never print the closing END marker (marker-less)
#   DIE_MIDWAY      if "1", exit non-zero mid-reply BEFORE the closing marker
#                   (models the target vanishing mid-call)
#   EXIT_AFTER_REPLY if "1", exit 0 immediately AFTER the fenced reply + closing
#                   marker instead of lingering at the idle prompt (models a
#                   target that answers cleanly then quits — must NOT be flagged
#                   as a mid-reply death)
#
# Dash-safe (CI shell is dash, see CLAUDE.md): multibyte glyphs are written
# LITERALLY, never as \xHH byte escapes; no bashisms.

delay_ms=${REPLY_DELAY_MS:-400}

# Sleep the given whole milliseconds. `sleep` is coreutils here and accepts a
# fractional-seconds argument; dash's printf handles the %03d zero-padding.
msleep() {
	_s=$(( $1 / 1000 ))
	_f=$(( $1 % 1000 ))
	sleep "$(printf '%d.%03d' "$_s" "$_f")"
}

# Banner chrome, modelled on tests/fixtures/claude_short.txt. Literal glyphs.
printf ' ▐▛███▜▌   Fake CLI v1.0\n'
printf '▝▜█████▛▘  slow-chrome test target\n'
printf '\n❯ \n'

# Read the (single-line) wrapped prompt. flat-cyborg types the command followed
# by CR; the PTY is in canonical mode, so `read` receives one cooked line that
# contains both sentinel markers (they appear in the wrap instruction).
IFS= read -r cmd || cmd=

# Pull the sentinel pair out of the prompt line, exactly as tests/cli.rs does.
b=
e=
for w in $cmd; do
	case $w in
	FCB_*_BEGIN) b=$w ;;
	FCB_*_END) e=$w ;;
	esac
done

# "Think" slowly, repainting an animated spinner + interrupt hint the whole
# time — a real TUI never falls silent on its own while working. Split the
# configured delay across a few frames.
frames=4
i=0
while [ "$i" -lt "$frames" ]; do
	case $(( i % 4 )) in
	0) g='✻' ;;
	1) g='✳' ;;
	2) g='✽' ;;
	*) g='✻' ;;
	esac
	printf '\r%s Thinking… (esc to interrupt)' "$g"
	msleep "$(( delay_ms / frames ))"
	i=$(( i + 1 ))
done
printf '\n'

if [ "$DIE_MIDWAY" = 1 ]; then
	# Target vanishes mid-reply: emit the opening marker and a partial line,
	# then exit non-zero BEFORE the closing marker. This is the "target died
	# mid-call" shape flat-cyborg must CLASSIFY (Completed-via-EOF,
	# un-interrupted), not the tool self-faulting.
	[ -n "$b" ] && printf '%s\n' "$b"
	printf 'partial repl'
	exit 1
fi

# Normal reply: fence the answer between the sentinel pair (closing marker on
# its own line is flat-cyborg's completion signal under --extract).
[ -n "$b" ] && printf '%s\n' "$b"
printf 'PONG\n'
if [ "$OMIT_MARKER" != 1 ] && [ -n "$e" ]; then
	printf '%s\n' "$e"
fi

if [ "$EXIT_AFTER_REPLY" = 1 ]; then
	# Clean answer, then quit: the reply was already fenced (the gate opened on
	# the closing marker before EOF), so flat-cyborg must complete on the marker
	# (Idle/exit 0) and NOT reclassify this as a mid-reply death (exit 75).
	exit 0
fi

if [ -n "$b" ]; then
	# --extract run: stay at an idle prompt so the ONLY completion signal
	# flat-cyborg sees is the closing sentinel — a target exiting here would
	# confound the measurement. flat-cyborg tears us down once it has the reply.
	printf '\n❯ '
	while IFS= read -r _line; do
		printf '\n❯ '
	done
fi
# Plain-capture run (no markers typed): nothing more to say; exit cleanly so
# the wrapper completes on EOF.
