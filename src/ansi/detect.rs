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
    fn detects_trailing_prompt_verbatim() {
        assert!(line_ends_with_any("welcome\n> ", &["> ", "$ "]));
        assert!(line_ends_with_any("user@host:~$ ", &["$ "]));
        assert!(!line_ends_with_any("still running...", &["> ", "$ "]));
        assert!(!line_ends_with_any("", &["> "]));
        // Verbatim matching avoids over-matching ordinary text.
        assert!(!line_ends_with_any("let v: Vec<T>", &["> "]));
    }
}
