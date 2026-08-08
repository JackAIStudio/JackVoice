/// Keep this aligned with AgenBoard's SpeechTranscriptNormalizer:
/// only a few fixed phrase replacements, no heavy cleanup.
pub fn normalize(text: &str) -> String {
    let mut output = text.to_string();
    let replacements = [
        ("斜杠 new", "/new"),
        ("斜杠new", "/new"),
        ("slash new", "/new"),
        ("斜杠 start", "/start"),
        ("斜杠start", "/start"),
        ("slash start", "/start"),
        ("open claw", "OpenClaw"),
        ("克劳德 code", "Claude Code"),
    ];

    for (pattern, replacement) in replacements {
        output = case_insensitive_replace(&output, pattern, replacement);
    }

    output
}

fn case_insensitive_replace(input: &str, pattern: &str, replacement: &str) -> String {
    let pattern_lower: Vec<char> = pattern.to_lowercase().chars().collect();
    if pattern_lower.is_empty() {
        return input.to_string();
    }

    let chars: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    let mut out = String::with_capacity(input.len());

    while i < chars.len() {
        if i + pattern_lower.len() <= chars.len() {
            let window_equal = chars[i..i + pattern_lower.len()]
                .iter()
                .map(|c| c.to_lowercase().next().unwrap_or(*c))
                .eq(pattern_lower.iter().copied());
            if window_equal {
                out.push_str(replacement);
                i += pattern_lower.len();
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn replaces_fixed_phrases() {
        assert_eq!(normalize("请打开 open claw"), "请打开 OpenClaw");
        assert_eq!(normalize("输入 斜杠 new"), "输入 /new");
        assert_eq!(normalize("克劳德 code 很好用"), "Claude Code 很好用");
    }
}
