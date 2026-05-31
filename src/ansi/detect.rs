// ---------------------------------------------------------------------------
// Detection primitives.
// ---------------------------------------------------------------------------

/// Returns the last non-empty line of `text`, or `""` if there is none.
fn last_non_empty_line(text: &str) -> &str {
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
}

/// Returns `true` if the last non-empty line of `text` looks like a yes/no
/// confirmation prompt: a bracketed or parenthesized group whose options
/// (split on `/` or `,`) include both a "yes" and a "no" choice — e.g.
/// `[y/n]`, `(Y/n)`, `[yes/no]`, `[y/N/a]`, `[y,N,a,q,?]`. A bare `y/n` or
/// `yes/no` (no brackets) is also accepted.
///
/// Only the last line is inspected, so an already-answered prompt scrolled up
/// in the buffer does not trigger a match.
pub fn is_confirmation_prompt(text: &str) -> bool {
    let line = last_non_empty_line(text).to_ascii_lowercase();

    for (open, close) in [('[', ']'), ('(', ')')] {
        let mut rest = line.as_str();
        while let Some(o) = rest.find(open) {
            let after = &rest[o + 1..];
            let Some(c) = after.find(close) else { break };
            let group = &after[..c];
            if group_is_yes_no(group) {
                return true;
            }
            rest = &after[c + 1..];
        }
    }

    // Bracket-less forms.
    line.contains("y/n") || line.contains("yes/no")
}

/// Whether a bracket group's options include both a yes and a no choice.
fn group_is_yes_no(group: &str) -> bool {
    let mut has_yes = false;
    let mut has_no = false;
    for opt in group.split(['/', ',']) {
        match opt.trim() {
            "y" | "yes" => has_yes = true,
            "n" | "no" => has_no = true,
            _ => {}
        }
    }
    has_yes && has_no
}

/// Returns `true` if `text` looks like an agent **approval / trust menu** —
/// the arrow-key, numbered menus that agentic CLIs (codex, claude) show for
/// actions their `[y/n]` auto-confirm cannot answer (e.g. codex confirming a
/// `git push`, or claude's "trust this folder" prompt).
///
/// Unlike [`is_confirmation_prompt`], this is **not** wired in by default: the
/// wrapper only consults it under the opt-in `--auto-approve` flag, because
/// confirming such a menu bypasses the agent's own safety gate.
///
/// Detection is conservative — it requires **both** signals, looking only at
/// the last ~20 non-blank lines so an already-dismissed menu scrolled up does
/// not match:
/// 1. a confirm-able default option: a line whose trimmed text starts with the
///    selection marker (`›`/`❯`) or `1.` and contains "yes" (case-insensitive);
/// 2. a confirm hint: a line containing "to confirm" (covers "press enter to
///    confirm" / "enter to confirm").
///
/// Both signals are required so ordinary prose or a plain `[y/n]` prompt never
/// triggers it.
pub fn is_approval_menu(text: &str) -> bool {
    const WINDOW: usize = 20;
    let tail: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .rev()
        .take(WINDOW)
        .map(|l| l.to_ascii_lowercase())
        .collect();

    let has_default_yes = tail.iter().any(|l| {
        let t = l.trim_start();
        let is_first_option = t.starts_with('\u{203a}') // ›
            || t.starts_with('\u{276f}') // ❯
            || t.starts_with("1.")
            || t.starts_with("1)");
        is_first_option && t.contains("yes")
    });

    let has_confirm_hint = tail.iter().any(|l| l.contains("to confirm"));

    has_default_yes && has_confirm_hint
}

/// Returns `true` if the last non-empty line of `text` ends with any of the
/// given prompt tokens, matched verbatim.
///
/// Used to recognize a Target CLI's trailing prompt. Tokens are matched exactly
/// (including any trailing space), so callers should pass distinctive tokens
/// such as `"> "` or `"$ "` rather than a bare `">"`, which would also match
/// ordinary text like `Vec<T>`.
pub fn line_ends_with_any(text: &str, tokens: &[&str]) -> bool {
    let line = last_non_empty_line(text);
    tokens.iter().any(|t| !t.is_empty() && line.ends_with(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_confirmation_prompts() {
        assert!(is_confirmation_prompt("Proceed? [y/n]"));
        assert!(is_confirmation_prompt("Overwrite file? (Y/n)"));
        assert!(is_confirmation_prompt("Delete all? (y/N) "));
        assert!(is_confirmation_prompt("Continue [yes/no]"));
        assert!(is_confirmation_prompt("Apply patch [y/N/a]?"));
        assert!(is_confirmation_prompt("Stage this hunk [y,n,q,a,d,e,?]?"));
        assert!(is_confirmation_prompt("really? y/n"));
        assert!(!is_confirmation_prompt("just some output"));
        assert!(!is_confirmation_prompt("the year was 1999"));
        assert!(!is_confirmation_prompt("pick a range [2/3]"));
    }

    #[test]
    fn confirmation_only_matches_last_line() {
        // An already-answered prompt scrolled up must not trigger.
        let scrollback = "Proceed? [y/n]\ny\nDone.";
        assert!(!is_confirmation_prompt(scrollback));
    }

    #[test]
    fn detects_codex_git_push_approval_menu() {
        let menu = "\
  Would you like to run the following command?
  Reason: push the branch to the remote
  $ git push origin add-test-md
\u{203a} 1. Yes, proceed (y)
  2. Yes, and don't ask again for commands that start with `git push` (p)
  3. No, and tell Codex what to do differently (esc)
  Press enter to confirm or esc to cancel";
        assert!(is_approval_menu(menu), "codex menu not detected");
    }

    #[test]
    fn detects_claude_trust_folder_menu() {
        let menu = "\
Do you trust the files in this folder?
\u{276f} 1. Yes, I trust this folder
  2. No, exit
Press enter to confirm or esc to cancel";
        assert!(is_approval_menu(menu), "claude trust menu not detected");
    }

    #[test]
    fn approval_menu_is_conservative() {
        // A plain shell prompt.
        assert!(!is_approval_menu("user@host:~$ "));
        // A bare [y/n] line (handled by is_confirmation_prompt, not this).
        assert!(!is_approval_menu("Proceed? [y/n]"));
        // Ordinary prose.
        assert!(!is_approval_menu(
            "Yes, the answer to your question is 42.\nPress enter when ready."
        ));
        // A numbered menu missing the confirm hint must NOT trigger.
        assert!(!is_approval_menu(
            "\u{203a} 1. Yes, proceed (y)\n  2. No, cancel"
        ));
        // A confirm hint without a yes-default option must NOT trigger.
        assert!(!is_approval_menu(
            "Choose an item.\nPress enter to confirm your selection."
        ));
    }

    #[test]
    fn detects_trailing_prompt_verbatim() {
        assert!(line_ends_with_any("welcome\n> ", &["> ", "$ "]));
        assert!(line_ends_with_any("user@host:~$ ", &["$ "]));
        assert!(!line_ends_with_any("still running...", &["> ", "$ "]));
        assert!(!line_ends_with_any("", &["> "]));
        // Verbatim matching avoids over-matching ordinary text.
        assert!(!line_ends_with_any("let v: Vec<T>", &["> "]));
    }
}
