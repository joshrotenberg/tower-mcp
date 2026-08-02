//! Quote-aware command-line tokenization for interactive and `--exec` input.
//!
//! This is intentionally smaller than a shell parser: quotes group one
//! argument, backslashes escape the next character outside single quotes,
//! and a top-level trailing `&` selects task-augmented execution. JSON object
//! and array literals remain byte-for-byte intact so raw `call` arguments and
//! schema-coerced object values do not lose their JSON quotes.

/// A tokenized REPL command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    /// Command name followed by its arguments.
    pub words: Vec<String>,
    /// Whether an unquoted trailing `&` requested task-augmented execution.
    pub background: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Quote {
    Single,
    Double,
}

#[derive(Debug)]
struct Word {
    text: String,
    protected: bool,
}

/// Split one command line without losing quoted whitespace or JSON syntax.
pub fn parse(line: &str) -> Result<ParsedCommand, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut word_started = false;
    let mut protected = false;
    let mut quote = None;
    let mut escaped = false;
    let mut json_stack = Vec::new();
    let mut json_string = false;
    let mut json_escaped = false;

    for ch in line.chars() {
        if !json_stack.is_empty() {
            current.push(ch);
            word_started = true;
            if json_string {
                if json_escaped {
                    json_escaped = false;
                } else if ch == '\\' {
                    json_escaped = true;
                } else if ch == '"' {
                    json_string = false;
                }
                continue;
            }
            match ch {
                '"' => json_string = true,
                '{' => json_stack.push('}'),
                '[' => json_stack.push(']'),
                '}' | ']' => {
                    let expected = json_stack.pop().expect("non-empty JSON stack");
                    if ch != expected {
                        return Err(format!(
                            "mismatched JSON delimiter: expected `{expected}`, found `{ch}`"
                        ));
                    }
                }
                _ => {}
            }
            continue;
        }

        if escaped {
            current.push(ch);
            word_started = true;
            protected = true;
            escaped = false;
            continue;
        }

        match quote {
            Some(Quote::Single) => {
                if ch == '\'' {
                    quote = None;
                } else {
                    current.push(ch);
                }
                word_started = true;
                protected = true;
            }
            Some(Quote::Double) => {
                if ch == '"' {
                    quote = None;
                } else if ch == '\\' {
                    escaped = true;
                } else {
                    current.push(ch);
                }
                word_started = true;
                protected = true;
            }
            None => match ch {
                '\\' => {
                    escaped = true;
                    word_started = true;
                    protected = true;
                }
                '\'' => {
                    quote = Some(Quote::Single);
                    word_started = true;
                    protected = true;
                }
                '"' => {
                    quote = Some(Quote::Double);
                    word_started = true;
                    protected = true;
                }
                '{' | '[' if current.is_empty() || current.ends_with('=') => {
                    current.push(ch);
                    word_started = true;
                    json_stack.push(if ch == '{' { '}' } else { ']' });
                }
                c if c.is_whitespace() => {
                    finish_word(&mut words, &mut current, &mut word_started, &mut protected);
                }
                _ => {
                    current.push(ch);
                    word_started = true;
                }
            },
        }
    }

    if escaped {
        return Err("trailing backslash escapes no character".to_string());
    }
    if let Some(quote) = quote {
        let name = match quote {
            Quote::Single => "single",
            Quote::Double => "double",
        };
        return Err(format!("unmatched {name} quote"));
    }
    if json_string {
        return Err("unterminated string in JSON argument".to_string());
    }
    if let Some(expected) = json_stack.last() {
        return Err(format!("unclosed JSON argument: expected `{expected}`"));
    }
    finish_word(&mut words, &mut current, &mut word_started, &mut protected);

    let background = words
        .last()
        .is_some_and(|word| word.text == "&" && !word.protected);
    if background {
        words.pop();
    }

    Ok(ParsedCommand {
        words: words.into_iter().map(|word| word.text).collect(),
        background,
    })
}

fn finish_word(
    words: &mut Vec<Word>,
    current: &mut String,
    word_started: &mut bool,
    protected: &mut bool,
) {
    if *word_started {
        words.push(Word {
            text: std::mem::take(current),
            protected: *protected,
        });
    }
    *word_started = false;
    *protected = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_group_whitespace_and_are_removed() {
        let parsed = parse(r#"tool a="hello world" b='single value'"#).unwrap();
        assert_eq!(parsed.words, ["tool", "a=hello world", "b=single value"]);
        assert!(!parsed.background);
    }

    #[test]
    fn escapes_quotes_backslashes_and_whitespace() {
        let parsed = parse(r#"tool a="say \"hi\"" path='C:\tmp' note=two\ words"#).unwrap();
        assert_eq!(
            parsed.words,
            ["tool", "a=say \"hi\"", "path=C:\\tmp", "note=two words"]
        );
    }

    #[test]
    fn json_objects_and_arrays_keep_their_quotes_and_spaces() {
        let parsed = parse(
            r#"call run.start {"instruction": "Reply with exactly hello", "items": [1, 2]} &"#,
        )
        .unwrap();
        assert_eq!(
            parsed.words,
            [
                "call",
                "run.start",
                r#"{"instruction": "Reply with exactly hello", "items": [1, 2]}"#,
            ]
        );
        assert!(parsed.background);
    }

    #[test]
    fn only_a_plain_trailing_ampersand_backgrounds() {
        assert!(parse("tool a=1 &").unwrap().background);
        assert!(!parse(r#"tool value="&""#).unwrap().background);
        assert_eq!(
            parse(r#"tool value="&""#).unwrap().words,
            ["tool", "value=&"]
        );
        assert_eq!(
            parse(r#"tool value=\&"#).unwrap().words,
            ["tool", "value=&"]
        );
    }

    #[test]
    fn malformed_quotes_and_escapes_fail_locally() {
        assert_eq!(
            parse(r#"tool a="unfinished"#).unwrap_err(),
            "unmatched double quote"
        );
        assert_eq!(
            parse("tool a=unfinished\\").unwrap_err(),
            "trailing backslash escapes no character"
        );
    }

    #[test]
    fn empty_and_unquoted_arguments_remain_compatible() {
        assert_eq!(
            parse("tool a=1 b=true").unwrap().words,
            ["tool", "a=1", "b=true"]
        );
        assert_eq!(parse(r#"tool empty="""#).unwrap().words, ["tool", "empty="]);
    }
}
