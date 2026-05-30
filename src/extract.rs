//! Deterministic, per-CLI structural extraction of an interactive LLM's reply
//! from a captured screen transcript.
//!
//! Unlike the sentinel-wrap approach (which asks the model to fence its answer
//! between unique markers — something some CLIs ignore for long replies), these
//! extractors slice the answer out of the transcript by recognizing each tool's
//! own chrome (prompt boxes, status lines, banners). They are therefore tied to
//! the specific TUI layout of each known CLI and may need updates if those UIs
//! change.
//!
//! [`extract_for_target`] dispatches on the program basename. Unknown targets
//! return `None`, leaving the caller to fall back to the sentinel path.

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
fn is_claude_bottom_chrome(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    if t.starts_with('✻') || t.starts_with('❯') || t.starts_with('⏵') || t.starts_with('›')
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

/// Extracts claude's reply: the indented block sitting above the bottom chrome
/// (`✻` status / horizontal rule / `❯` / `⏵`), stopping at a banner, a
/// non-indented line, or an echoed-prompt marker above it.
pub(crate) fn extract_claude(transcript: &str) -> Option<String> {
    let lines: Vec<&str> = transcript.lines().collect();
    if lines.is_empty() {
        return None;
    }

    // Walk up from the bottom across the trailing chrome/blank region; the reply
    // ends just above it. If no chrome is found, end at the last non-blank line.
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
    for (idx, &line) in lines[top..end].iter().enumerate() {
        if line.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let dedented = dedent(line, 2);
        if idx == 0 {
            // Strip a leading `●`/`● ` marker from the first reply line.
            let stripped = dedented
                .strip_prefix("● ")
                .or_else(|| dedented.strip_prefix('●'))
                .unwrap_or(dedented);
            out.push(stripped.to_string());
        } else {
            out.push(dedented.to_string());
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

#[cfg(test)]
mod tests {
    use super::*;

    const CODEX_SHORT: &str = include_str!("../tests/fixtures/codex_short.txt");
    const CODEX_LONG: &str = include_str!("../tests/fixtures/codex_long.txt");
    const CLAUDE_POEM: &str = include_str!("../tests/fixtures/claude_poem.txt");

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
}
