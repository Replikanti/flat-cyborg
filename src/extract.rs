//! Reply extraction for `--extract`: sentinel-first, sentinel-strict by default.
//!
//! The primary (and default) path is the sentinel: the prompt is wrapped with
//! unique per-run markers and [`extract_between`] slices the fenced reply out of
//! the captured transcript. The markers are self-validating — if they are
//! present we have high confidence in the result; if they are absent the default
//! is to return `None` (the caller prints nothing and warns), so a malformed or
//! refusal reply is empty downstream rather than a guess.
//!
//! Only when the caller opts in (`allow_structural`, the `--extract-structural`
//! flag) and a known CLI omitted the markers do we fall back to *structural*
//! extraction: slicing the answer out of the transcript by recognizing each
//! tool's own chrome (prompt boxes, status lines, banners). That approach is
//! tied to each CLI's TUI layout and inherently fragile — even with the strict
//! [`looks_clean`] gate it can pass echoed wrap-instruction prose on a refusal —
//! so it is off by default and best-effort only.
//!
//! [`choose_reply`] implements that decision as a pure function so it is
//! testable without a PTY. [`extract_for_target`] dispatches the structural
//! step on the program basename; unknown targets return `None`.

/// Removes up to `n` leading spaces from a line (a bounded dedent).
fn dedent(line: &str, n: usize) -> &str {
    let mut rest = line;
    for _ in 0..n {
        match rest.strip_prefix(' ') {
            Some(r) => rest = r,
            None => break,
        }
    }
    rest
}

/// Joins reply lines, trimming leading and trailing blank lines, returning
/// `None` if nothing remains.
fn finish(lines: Vec<String>) -> Option<String> {
    let mut start = 0;
    let mut end = lines.len();
    while start < end && lines[start].trim().is_empty() {
        start += 1;
    }
    while end > start && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    if start >= end {
        return None;
    }
    let text = lines[start..end].join("\n");
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// True if a codex transcript line is chrome that bounds the reply block.
fn is_codex_chrome(line: &str) -> bool {
    let t = line.trim_start();
    let t = t.trim_end();
    if t.is_empty() {
        return false;
    }
    let first = t.chars().next().unwrap();
    if matches!(first, '›' | '╭' | '│' | '╰' | '╮' | '╯' | '─') {
        return true;
    }
    if t.contains(" default · ") || t.starts_with("gpt-") {
        return true;
    }
    false
}

/// Extracts codex's reply: the `•`-bulleted block, including indented
/// continuation lines, stopping at the next chrome.
pub(crate) fn extract_codex(transcript: &str) -> Option<String> {
    let lines: Vec<&str> = transcript.lines().collect();

    // The reply starts at the LAST bullet line (earlier `›` lines may echo the
    // user's prompt; a later `•` is the model's actual answer).
    let start = lines.iter().enumerate().rev().find_map(|(i, l)| {
        let t = l.trim_start();
        (t == "•" || t.starts_with("• ")).then_some(i)
    })?;

    let mut out: Vec<String> = Vec::new();
    // The bullet line itself: drop the leading `•` and following spaces.
    let bullet = lines[start].trim_start();
    let head = bullet.strip_prefix('•').unwrap_or(bullet).trim_start();
    out.push(head.to_string());

    // Continuation lines: blank, or >=2-space indent and not chrome. Stop at the
    // first chrome line.
    for &line in &lines[start + 1..] {
        if line.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        if is_codex_chrome(line) {
            break;
        }
        if line.starts_with("  ") {
            out.push(dedent(line, 2).to_string());
        } else {
            break;
        }
    }

    finish(out)
}

/// True if a claude transcript line is part of the bottom chrome region.
///
/// Recognizes the status spinner (`✻`), the prompt box (`❯` / `›`), the mode
/// line (`⏵`), the horizontal rules, and the additional bottom banners seen in
/// live runs on short replies: the auto-mode line, the weekly-limit notice, and
/// the in-flight "esc to interrupt" hint.
fn is_claude_bottom_chrome(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    if t.starts_with('✻') || t.starts_with('❯') || t.starts_with('⏵') || t.starts_with('›')
    {
        return true;
    }
    if t.contains("auto mode on")
        || t.contains("weekly limit")
        || t.contains("You've used")
        || t.contains("esc to interrupt")
    {
        return true;
    }
    // Horizontal rule: only box-drawing dashes, reasonably long.
    if t.len() >= 10 && t.chars().all(|c| c == '─') {
        return true;
    }
    false
}

/// True if a claude line is banner chrome (above the reply).
fn is_claude_banner(line: &str) -> bool {
    line.contains('▐')
        || line.contains('▛')
        || line.contains("Claude Code v")
        || line.contains("Welcome")
        || line.contains("Tips for getting started")
        || line.contains("What's new")
}

/// Extracts claude's reply. Two layouts are seen live:
///
/// 1. A `●` answer bullet is rendered (short replies): anchor on the LAST `●`
///    line and collect downward until the bottom chrome.
/// 2. No bullet is rendered (e.g. long replies where it scrolled off): take the
///    indented block sitting just above the bottom chrome (`✻` status / rules /
///    `❯` prompt / `⏵` mode / banners), walking up while lines are blank or
///    `>=2`-space indented and not a banner.
///
/// Lines are dedented by up to two spaces so an indented variant reads cleanly.
pub(crate) fn extract_claude(transcript: &str) -> Option<String> {
    let lines: Vec<&str> = transcript.lines().collect();
    if lines.is_empty() {
        return None;
    }

    // Layout 1: a `●` answer bullet is present — anchor on the LAST one and
    // collect downward.
    if let Some(start) = lines.iter().enumerate().rev().find_map(|(i, l)| {
        let t = l.trim_start();
        (t == "●" || t.starts_with("● ")).then_some(i)
    }) {
        let mut out: Vec<String> = Vec::new();
        let bullet = lines[start].trim_start();
        let head = bullet.strip_prefix('●').unwrap_or(bullet).trim_start();
        out.push(head.to_string());
        for &line in &lines[start + 1..] {
            if line.trim().is_empty() {
                out.push(String::new());
                continue;
            }
            if is_claude_bottom_chrome(line) || is_claude_banner(line) {
                break;
            }
            out.push(dedent(line, 2).to_string());
        }
        return finish(out);
    }

    // Layout 2: no bullet. Walk up from the bottom across the trailing
    // chrome/blank region; the reply ends just above it.
    let mut end = lines.len(); // exclusive index of reply end
    while end > 0 {
        let line = lines[end - 1];
        if line.trim().is_empty() || is_claude_bottom_chrome(line) {
            end -= 1;
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }

    // Walk up collecting reply lines: blank or >=2-space indent. Stop at a
    // non-indented non-blank line, a banner, or an echoed-prompt marker.
    let mut top = end;
    while top > 0 {
        let line = lines[top - 1];
        let t = line.trim();
        if t.is_empty() {
            top -= 1;
            continue;
        }
        if is_claude_banner(line) || t.starts_with('❯') || t.starts_with('›') {
            break;
        }
        if line.starts_with("  ") {
            top -= 1;
        } else {
            break;
        }
    }

    let mut out: Vec<String> = Vec::new();
    for &line in &lines[top..end] {
        if line.trim().is_empty() {
            out.push(String::new());
        } else {
            out.push(dedent(line, 2).to_string());
        }
    }

    finish(out)
}

/// Dispatches structural extraction on the basename of `program`. Returns `None`
/// for unknown targets so the caller can fall back to the sentinel path.
pub(crate) fn extract_for_target(program: &str, transcript: &str) -> Option<String> {
    let base = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    match base.as_str() {
        "claude" => extract_claude(transcript),
        "codex" => extract_codex(transcript),
        _ => None,
    }
}

/// Strict sanity gate for a structural-fallback result. Structural extraction is
/// best-effort and tied to each CLI's layout, so before printing its output we
/// reject anything that still smells of UI chrome (box drawing, status glyphs,
/// banner substrings), is empty, or has a runaway line. A clean reply passes; a
/// dirty one is treated as "no result" so the caller warns instead of emitting
/// garbage.
pub(crate) fn looks_clean(s: &str) -> bool {
    if s.trim().is_empty() {
        return false;
    }
    // Chrome glyphs that should never appear in a real reply.
    const CHROME_CHARS: &[char] = &['✻', '❯', '⏵', '›', '╭', '│', '╰', '╮', '╯', '●', '•'];
    if s.contains(CHROME_CHARS) {
        return false;
    }
    // A run of 3+ box-drawing dashes (horizontal rule).
    if s.contains("───") {
        return false;
    }
    // Banner / status substrings.
    const CHROME_SUBSTRINGS: &[&str] = &[
        "auto mode",
        "weekly limit",
        "gpt-",
        "/model",
        "Claude Code v",
        "Tips for getting started",
        "esc to interrupt",
        "for agents",
        // A leaked/corrupted sentinel fragment means the fence broke; fail the
        // gate so we warn rather than print a half-marked reply.
        "FCB_",
    ];
    if CHROME_SUBSTRINGS.iter().any(|m| s.contains(m)) {
        return false;
    }
    // A runaway line (no real reply line should be this long).
    if s.lines().any(|l| l.len() > 400) {
        return false;
    }
    true
}

/// Decides the reply to emit. This is the pure core of the `--extract` decision
/// so it can be tested without a PTY:
///
/// 1. If the sentinel markers are present, return the fenced text (high
///    confidence — the markers are self-validating). This is the default,
///    strict path.
/// 2. Otherwise, **only when `allow_structural`** (the `--extract-structural`
///    opt-in), try structural extraction for a known CLI and accept it ONLY if
///    it passes [`looks_clean`].
/// 3. Otherwise return `None` (the caller warns and prints nothing).
///
/// The structural slice is best-effort and tied to each CLI's chrome; on a
/// refusal/clarification it can scrape echoed *wrap-instruction prose* (no
/// chrome glyph, so [`looks_clean`] passes it) and hand a consumer garbage that
/// is indistinguishable from a real reply. So it is gated behind an explicit
/// opt-in; by default a missing fence is treated as no-reply (== empty
/// downstream), never a structural scrape.
pub(crate) fn choose_reply(
    program: &str,
    transcript: &str,
    begin: &str,
    end: &str,
    allow_structural: bool,
) -> Option<String> {
    if let Some(fenced) = extract_between(transcript, begin, end) {
        return Some(fenced);
    }
    if allow_structural {
        return extract_for_target(program, transcript).filter(|s| looks_clean(s));
    }
    None
}

/// Extracts the text between the LAST begin/end marker pair in `text`. Using the
/// last pair skips the echoed instruction (which appears earlier in the
/// transcript) and grabs the model's actual fenced reply. Returns `None` if
/// either marker is missing.
fn extract_between(text: &str, begin: &str, end: &str) -> Option<String> {
    let e = text.rfind(end)?; // last END
    let b = text[..e].rfind(begin)?; // last BEGIN before it
    let inner = &text[b + begin.len()..e];
    Some(inner.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEX_SHORT: &str = include_str!("../tests/fixtures/codex_short.txt");
    const CODEX_LONG: &str = include_str!("../tests/fixtures/codex_long.txt");
    const CLAUDE_POEM: &str = include_str!("../tests/fixtures/claude_poem.txt");
    const CLAUDE_SHORT: &str = include_str!("../tests/fixtures/claude_short.txt");

    #[test]
    fn codex_short_yields_single_word() {
        assert_eq!(extract_codex(CODEX_SHORT).as_deref(), Some("pineapple"));
    }

    #[test]
    fn codex_long_yields_full_poem() {
        let got = extract_codex(CODEX_LONG).expect("reply");
        assert!(
            got.starts_with("Morning folds its quiet map"),
            "got: {got:?}"
        );
        // The captured reply is the bullet line plus its indented continuation:
        // 19 verse lines plus the trailing `ZZEND9` token line = 20 lines.
        assert_eq!(got.lines().count(), 20, "got: {got:?}");
        assert!(got.ends_with("ZZEND9"), "got: {got:?}");
        assert!(!got.contains('›'), "leaked prompt chrome: {got:?}");
        assert!(!got.contains("gpt-"), "leaked status chrome: {got:?}");
        assert!(!got.contains('•'), "leaked bullet: {got:?}");
    }

    #[test]
    fn claude_poem_yields_full_poem() {
        let got = extract_claude(CLAUDE_POEM).expect("reply");
        assert!(got.starts_with("The Long Way Home"), "got: {got:?}");
        assert!(
            got.ends_with("I'll know that home was where I'd come."),
            "got: {got:?}"
        );
        assert!(!got.contains('✻'), "leaked status chrome: {got:?}");
        assert!(!got.contains('─'), "leaked rule: {got:?}");
        assert!(!got.contains('❯'), "leaked prompt: {got:?}");
        assert!(!got.contains('⏵'), "leaked mode line: {got:?}");
    }

    #[test]
    fn dispatch_routes_known_targets() {
        assert_eq!(
            extract_for_target("claude", CLAUDE_POEM)
                .as_deref()
                .map(|s| s.starts_with("The Long Way Home")),
            Some(true)
        );
        assert_eq!(
            extract_for_target("/usr/local/bin/codex", CODEX_SHORT).as_deref(),
            Some("pineapple")
        );
    }

    #[test]
    fn dispatch_unknown_target_is_none() {
        assert_eq!(extract_for_target("bash", CODEX_SHORT), None);
        assert_eq!(extract_for_target("/bin/zsh", CLAUDE_POEM), None);
    }

    // The live fixture that broke the previous structural-first design: a short
    // claude reply whose bottom chrome is the auto-mode / 1-MCP banner (not the
    // usual `✻ Churned…`). With the `●`-anchored extractor it yields cleanly.
    #[test]
    fn claude_short_yields_single_word() {
        assert_eq!(extract_claude(CLAUDE_SHORT).as_deref(), Some("pineapple"));
    }

    #[test]
    fn claude_short_result_is_clean() {
        let got = extract_claude(CLAUDE_SHORT).expect("reply");
        assert!(looks_clean(&got), "should pass sanity gate: {got:?}");
    }

    #[test]
    fn looks_clean_accepts_normal_answer() {
        assert!(looks_clean("First line of the answer.\nSecond line here."));
        assert!(looks_clean("pineapple"));
    }

    #[test]
    fn looks_clean_rejects_empty() {
        assert!(!looks_clean(""));
        assert!(!looks_clean("   \n  \t "));
    }

    #[test]
    fn looks_clean_rejects_chrome_glyphs() {
        for g in ['✻', '❯', '⏵', '›', '╭', '│', '╰', '╮', '╯', '●', '•'] {
            let s = format!("answer {g} more");
            assert!(!looks_clean(&s), "should reject glyph {g:?}");
        }
    }

    #[test]
    fn looks_clean_rejects_horizontal_rule() {
        assert!(!looks_clean("answer\n────────────────\nmore"));
        // A run of exactly 3 dashes is enough.
        assert!(!looks_clean("a───b"));
    }

    #[test]
    fn looks_clean_rejects_banner_substrings() {
        for m in [
            "auto mode on (shift+tab to cycle)",
            "You've used 80% of your weekly limit",
            "gpt-5-codex",
            "type /model to change",
            "Claude Code v2.1.158",
            "Tips for getting started",
            "esc to interrupt",
            "← for agents",
        ] {
            assert!(!looks_clean(m), "should reject banner: {m:?}");
        }
    }

    #[test]
    fn looks_clean_rejects_leaked_marker() {
        // A leaked/corrupted sentinel fragment must fail the gate.
        assert!(!looks_clean("answer FCB_x_BEGIN more"));
    }

    #[test]
    fn looks_clean_rejects_runaway_line() {
        let long = "x".repeat(401);
        assert!(!looks_clean(&long));
        // A 400-char line is still acceptable (boundary is > 400).
        let ok = "y".repeat(400);
        assert!(looks_clean(&ok));
    }

    #[test]
    fn extract_between_picks_last_pair() {
        // The transcript contains the echoed instruction (an earlier mention of
        // the markers in prose) followed by the model's real fenced reply.
        let begin = "FCB_abc123_BEGIN";
        let end = "FCB_abc123_END";
        let transcript = format!(
            "> summarize\n\nIMPORTANT: wrap between {begin} and {end}.\n\
             {begin}\nLine one of the answer.\nLine two of the answer.\n{end}\n"
        );
        let got = extract_between(&transcript, begin, end).unwrap();
        assert_eq!(got, "Line one of the answer.\nLine two of the answer.");
    }

    #[test]
    fn extract_between_multiline_reply_retained() {
        let begin = "FCB_x_BEGIN";
        let end = "FCB_x_END";
        let body: Vec<String> = (0..50).map(|i| format!("answer line {i}")).collect();
        let joined = body.join("\n");
        let transcript = format!("noise before\n{begin}\n{joined}\n{end}\ntrailing noise");
        let got = extract_between(&transcript, begin, end).unwrap();
        assert_eq!(got, joined);
    }

    #[test]
    fn extract_between_missing_markers_is_none() {
        assert_eq!(extract_between("no markers here", "B", "E"), None);
    }

    #[test]
    fn extract_between_only_one_marker_is_none() {
        assert_eq!(extract_between("FCB_B reply text", "FCB_B", "FCB_E"), None);
        assert_eq!(extract_between("reply text FCB_E", "FCB_B", "FCB_E"), None);
    }

    // --- The decision: sentinel-first; structural only when opted in. ---

    #[test]
    fn choose_reply_prefers_sentinel_even_with_chrome_present() {
        let begin = "FCB_z_BEGIN";
        let end = "FCB_z_END";
        // The transcript has both fenced markers AND claude chrome elsewhere; the
        // fenced text must win, untouched — in BOTH modes.
        let transcript = format!(
            "● some chatter\n✻ Brewed for 1s\n{begin}\nThe real answer.\n{end}\n\
             ────────────\n❯\n  ⏵⏵ auto mode on"
        );
        assert_eq!(
            choose_reply("claude", &transcript, begin, end, false).as_deref(),
            Some("The real answer.")
        );
        assert_eq!(
            choose_reply("claude", &transcript, begin, end, true).as_deref(),
            Some("The real answer.")
        );
    }

    #[test]
    fn choose_reply_falls_back_to_clean_structural_only_when_opted_in() {
        // No markers, but the claude block is clean → structural fallback ONLY
        // with allow_structural=true.
        assert_eq!(
            choose_reply("claude", CLAUDE_SHORT, "NOPE_BEGIN", "NOPE_END", true).as_deref(),
            Some("pineapple")
        );
    }

    #[test]
    fn choose_reply_strict_default_ignores_even_a_clean_structural_slice() {
        // The #42 fix: by default (allow_structural=false), a missing fence is
        // no-reply, NOT a structural scrape — even when the slice would be clean.
        assert_eq!(
            choose_reply("claude", CLAUDE_SHORT, "NOPE_BEGIN", "NOPE_END", false),
            None
        );
    }

    #[test]
    fn choose_reply_strict_default_ignores_echoed_instruction_prose() {
        // The exact #42 repro: a refusal leaves only echoed wrap-instruction prose
        // (no chrome glyph, so looks_clean passes it). Strict mode must return
        // None rather than hand back that fragment.
        let transcript = "● I can't help with that.\n  on its own line before it and the marker\n❯";
        assert_eq!(
            choose_reply("claude", transcript, "NOPE_BEGIN", "NOPE_END", false),
            None
        );
    }

    #[test]
    fn choose_reply_rejects_garbage_structural() {
        // Even with structural opted in: no markers and the "reply" is chrome —
        // there is no `●` bullet so structural returns None anyway, and a
        // chrome-laden block would be filtered by looks_clean. Either way: None.
        let transcript = "✻ Brewed for 1s\n────────────\n❯\n  ⏵⏵ auto mode on (shift+tab to cycle)";
        assert_eq!(
            choose_reply("claude", transcript, "NOPE_BEGIN", "NOPE_END", true),
            None
        );
    }

    #[test]
    fn choose_reply_unknown_target_without_markers_is_none() {
        assert_eq!(
            choose_reply("bash", "just some output\n", "NOPE_BEGIN", "NOPE_END", true),
            None
        );
    }
}
