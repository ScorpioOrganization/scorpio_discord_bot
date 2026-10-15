//! Text helpers shared by modules that post user configured messages.

/// Replaces `{token}` placeholders using `replace`. Tokens it returns None for, and braces
/// that do not form a token, are kept as typed.
pub fn render(template: &str, mut replace: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;

    while let Some(start) = rest.find('{') {
        let Some(len) = rest[start..].find('}') else {
            break;
        };
        let end = start + len;
        out.push_str(&rest[..start]);
        match replace(&rest[start + 1..end]) {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[start..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Escapes Discord markdown so names and titles cannot change the message formatting.
pub fn escape_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '*' | '_' | '~' | '`' | '|' | '>' | '#' | '[' | ']' | '(' | ')'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Cuts `s` to at most `max_chars` characters.
pub fn truncate_chars(mut s: String, max_chars: usize) -> String {
    if let Some((idx, _)) = s.char_indices().nth(max_chars) {
        s.truncate(idx);
    }
    s
}

/// `#rrggbb` to a number, as sent by `<input type="color">`.
pub fn parse_hex_color(raw: &str) -> Option<u32> {
    let hex = raw.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_uses_closure_and_keeps_unknown() {
        let out = render("{a} {b} {", |t| (t == "a").then(|| "A".to_owned()));
        assert_eq!(out, "A {b} {");
    }

    #[test]
    fn colors() {
        assert_eq!(parse_hex_color("#7f8cff"), Some(0x7f8cff));
        assert_eq!(parse_hex_color(" #000000 "), Some(0));
        assert_eq!(parse_hex_color("7f8cff"), None);
        assert_eq!(parse_hex_color("#fff"), None);
        assert_eq!(parse_hex_color("#gggggg"), None);
        assert_eq!(parse_hex_color("#+12345"), None);
    }

    #[test]
    fn truncate_is_char_based() {
        assert_eq!(truncate_chars("zażółć".into(), 3), "zaż");
        assert_eq!(truncate_chars("ab".into(), 5), "ab");
    }
}
