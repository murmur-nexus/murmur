//! How a command string becomes an argv when no shell runs it: the shell tool's non-interpreter
//! commands, plan steps, session checks and `lifecycle.done_check.command` all split with
//! [`split_shell_words`], so a command reads the same wherever it is written.

/// Split `command` into words: whitespace separates words, a `'` or `"` groups everything up to
/// its matching quote into the current word, and `\` takes the next character literally. Nothing
/// is expanded. An unclosed quote runs to the end of the string.
pub fn split_shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;

    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        match (ch, quote) {
            ('"' | '\'', None) => quote = Some(ch),
            (q, Some(open)) if q == open => quote = None,
            (c, None) if c.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::split_shell_words;

    fn words(command: &str) -> Vec<String> {
        split_shell_words(command)
    }

    #[test]
    fn whitespace_separates_words() {
        assert_eq!(
            words("  cargo   test\t--quiet\n"),
            ["cargo", "test", "--quiet"]
        );
        assert!(words("   ").is_empty());
    }

    #[test]
    fn quotes_group_words_and_are_dropped() {
        assert_eq!(
            words("bash -c 'echo a; exit 3'"),
            ["bash", "-c", "echo a; exit 3"]
        );
        assert_eq!(words(r#"say "it's here""#), ["say", "it's here"]);
        assert_eq!(words("a'b c'd"), ["ab cd"]);
    }

    #[test]
    fn a_backslash_takes_the_next_character_literally() {
        assert_eq!(words(r"a\ b c"), ["a b", "c"]);
        assert_eq!(words(r#"echo \"x\""#), ["echo", "\"x\""]);
    }

    #[test]
    fn nothing_is_expanded() {
        assert_eq!(words("echo $HOME *"), ["echo", "$HOME", "*"]);
    }

    #[test]
    fn an_unclosed_quote_runs_to_the_end() {
        assert_eq!(words("bash -c 'true"), ["bash", "-c", "true"]);
    }
}
